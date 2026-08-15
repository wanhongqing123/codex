use serde::Deserialize;
use serde_json::json;
use std::collections::VecDeque;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::net::Shutdown;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use url::Url;

use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::approval_events::ExecApprovalRequestEvent;

#[derive(Clone, Debug)]
struct BridgeConfig {
    host: String,
    port: u16,
    token: String,
}

#[derive(Default)]
pub(crate) struct RemoteImReplyDisplayFilter {
    source: String,
    visible: String,
}

impl RemoteImReplyDisplayFilter {
    pub(crate) fn push(&mut self, delta: &str) -> String {
        self.source.push_str(delta);
        let next = visible_remote_im_reply_text(&self.source);
        if !next.starts_with(&self.visible) {
            self.visible = next;
            return String::new();
        }
        let added = next[self.visible.len()..].to_string();
        self.visible = next;
        added
    }

    pub(crate) fn reset(&mut self) {
        self.source.clear();
        self.visible.clear();
    }
}

const REMOTE_IM_OPEN_PREFIX: &str = "<remote-im-reply";
const REMOTE_IM_CLOSE_PREFIX: &str = "</remote-im-reply";
const GENERATED_REPLY_ID_PREFIX: &str = "rim-";
const GENERATED_REPLY_ID_HEX_LEN: usize = 16;

pub(crate) fn remote_im_reply_id(text: &str) -> Option<String> {
    const PREFIX: &str = "Opening marker: <remote-im-reply id=\"";
    text.lines().find_map(|line| {
        let reply_id = line.trim().strip_prefix(PREFIX)?.strip_suffix("\">")?;
        (!reply_id.is_empty()
            && reply_id.len() <= 80
            && reply_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
        .then(|| reply_id.to_string())
    })
}

pub(crate) fn remote_im_task_id(text: &str) -> Option<String> {
    const PREFIX: &str = "Task marker: <remote-im-task id=\"";
    text.lines().find_map(|line| {
        let task_id = line.trim().strip_prefix(PREFIX)?.strip_suffix("\">")?;
        (!task_id.is_empty()
            && task_id.len() <= 160
            && task_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
        .then(|| task_id.to_string())
    })
}

fn generated_reply_id_end(marker: &str, value_start: usize) -> Option<usize> {
    let value_len = GENERATED_REPLY_ID_PREFIX.len() + GENERATED_REPLY_ID_HEX_LEN;
    let value_end = value_start.checked_add(value_len)?;
    let value = marker.get(value_start..value_end)?;
    let hex = value.strip_prefix(GENERATED_REPLY_ID_PREFIX)?;
    hex.bytes()
        .all(|byte| byte.is_ascii_hexdigit())
        .then_some(value_end)
}

fn remote_im_open_marker_body_start(marker: &str) -> Option<usize> {
    let suffix = marker.strip_prefix(REMOTE_IM_OPEN_PREFIX)?;
    if suffix.starts_with('>') {
        return Some(REMOTE_IM_OPEN_PREFIX.len() + 1);
    }

    const ID_PREFIX: &str = " id=\"";
    let with_id = suffix.strip_prefix(ID_PREFIX)?;
    if let Some(close) = with_id.find("\">") {
        return Some(REMOTE_IM_OPEN_PREFIX.len() + ID_PREFIX.len() + close + 2);
    }

    // Remote IM reply IDs are generated as rim- + 16 hex characters. Knowing
    // their exact length lets the TUI recover when a model omits the final
    // quote/angle bracket and starts the reply body immediately after the ID.
    let value_start = REMOTE_IM_OPEN_PREFIX.len() + ID_PREFIX.len();
    let mut body_start = generated_reply_id_end(marker, value_start)?;
    if marker
        .get(body_start..)
        .is_some_and(|tail| tail.starts_with('"'))
    {
        body_start += 1;
    }
    if marker
        .get(body_start..)
        .is_some_and(|tail| tail.starts_with('>'))
    {
        body_start += 1;
    }
    Some(body_start)
}

fn remote_im_reply_body_end(body: &str) -> usize {
    if let Some(index) = body.find(REMOTE_IM_CLOSE_PREFIX) {
        return index;
    }

    let max_partial = body.len().min(REMOTE_IM_CLOSE_PREFIX.len());
    for len in (1..=max_partial).rev() {
        if body
            .as_bytes()
            .ends_with(&REMOTE_IM_CLOSE_PREFIX.as_bytes()[..len])
        {
            return body.len() - len;
        }
    }
    body.len()
}

pub(crate) fn visible_remote_im_reply_text(text: &str) -> String {
    if let Some(open_index) = text.find(REMOTE_IM_OPEN_PREFIX) {
        let marker = &text[open_index..];
        let Some(body_start) = remote_im_open_marker_body_start(marker) else {
            return String::new();
        };
        let body = &marker[body_start..];
        let body = body
            .strip_prefix("\r\n")
            .or_else(|| body.strip_prefix('\n'))
            .unwrap_or(body);
        return body[..remote_im_reply_body_end(body)].to_string();
    }

    let candidate = text.trim_start();
    if !candidate.contains('\n') && REMOTE_IM_OPEN_PREFIX.starts_with(candidate) {
        return String::new();
    }
    text.to_string()
}

static BRIDGE_CONFIG: OnceLock<Option<BridgeConfig>> = OnceLock::new();

// Lazily-started data-channel manager. All codex -> host output (assistant_text /
// turn_error / control_result) is funneled through this so a single owner thread controls the
// TCP stream, tracks per-message acks and reconnects + resends on a stale socket.
static DATA_SENDER: OnceLock<Option<mpsc::Sender<DataMsg>>> = OnceLock::new();

// If the host does not ack an assistant_text within this window we treat the data
// socket as stale (e.g. half-open where write() keeps succeeding into a black
// hole), reconnect and resend. This is what stops the "回传出现后一直丢、必须重启
// AICLI 才恢复" sticky failure.
const ACK_TIMEOUT: Duration = Duration::from_millis(1500);
const WATCHDOG_TICK: Duration = Duration::from_millis(500);
const MAX_RESEND: usize = 8;
const MAX_PENDING: usize = 256;

pub(crate) fn init(endpoint: Option<String>) {
    let config = endpoint.and_then(|value| parse_endpoint(&value));
    let _ = BRIDGE_CONFIG.set(config);
}

#[derive(Deserialize)]
struct ControlAttachment {
    #[serde(rename = "type")]
    attachment_type: String,
    #[serde(rename = "localPath")]
    local_path: String,
    #[serde(rename = "mimeType")]
    mime_type: String,
}

#[derive(Deserialize)]
struct ControlPayload {
    token: Option<String>,
    kind: Option<String>,
    command: Option<String>,
    mode: Option<String>,
    model: Option<String>,
    reasoning: Option<String>,
    goal: Option<String>,
    task: Option<String>,
    text: Option<String>,
    #[serde(rename = "displayText")]
    display_text: Option<String>,
    #[serde(rename = "inputOrigin")]
    input_origin: Option<String>,
    #[serde(default)]
    attachments: Vec<ControlAttachment>,
    // 运行时主题：宿主终端的背景/前景色（6 位十六进制，可带 #）。
    bg: Option<String>,
    fg: Option<String>,
    #[serde(rename = "replyId")]
    reply_id: Option<String>,
    #[serde(rename = "taskId")]
    task_id: Option<String>,
    #[serde(rename = "threadId")]
    thread_id: Option<String>,
    #[serde(rename = "turnId")]
    turn_id: Option<String>,
    #[serde(rename = "approvalId")]
    approval_id: Option<String>,
    decision: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
}

/// Parse an `RRGGBB` (optionally `#`-prefixed) hex string into an 8-bit RGB tuple.
fn parse_hex_rgb(value: &str) -> Option<(u8, u8, u8)> {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

pub(crate) fn start_control_listener(app_event_tx: AppEventSender) {
    let Some(config) = BRIDGE_CONFIG.get().and_then(|item| item.as_ref()).cloned() else {
        return;
    };

    // Keep the IM control channel alive for the lifetime of the TUI session.
    // A transient TCP disconnect should not permanently disable /status, /model,
    // or mode-switch commands from remote IM.
    thread::spawn(move || {
        let mut first_attempt = true;
        loop {
            if !first_attempt {
                thread::sleep(std::time::Duration::from_secs(3));
            }
            first_attempt = false;

            let Ok(mut stream) = TcpStream::connect((config.host.as_str(), config.port)) else {
                continue;
            };
            let ready = json!({
                "token": config.token,
                "kind": "control_ready",
            });
            if stream.write_all(format!("{ready}\n").as_bytes()).is_err() {
                continue;
            }

            let Ok(reader_stream) = stream.try_clone() else {
                continue;
            };
            let reader = BufReader::new(reader_stream);
            for line in reader.lines().map_while(Result::ok) {
                let Ok(payload) = serde_json::from_str::<ControlPayload>(&line) else {
                    continue;
                };
                if let Some(event) = control_payload_to_app_event(payload, &config.token) {
                    app_event_tx.send(event);
                }
            }
        }
    });
}

fn control_payload_to_app_event(payload: ControlPayload, token: &str) -> Option<AppEvent> {
    if payload.token.as_deref() != Some(token) || payload.kind.as_deref() != Some("control") {
        return None;
    }

    match payload.command.as_deref()? {
        "switch_mode" => {
            let mode = match payload.mode.as_deref()? {
                "plan" => ModeKind::Plan,
                "build" => ModeKind::Default,
                _ => return None,
            };
            Some(AppEvent::MultiAiCodeImSwitchMode {
                mode,
                request_id: payload.request_id,
            })
        }
        "status" => Some(AppEvent::MultiAiCodeImStatus {
            request_id: payload.request_id?,
        }),
        "model" => Some(AppEvent::MultiAiCodeImModel {
            request_id: payload.request_id?,
            model: payload.model,
            reasoning: payload.reasoning,
        }),
        "goal" => Some(AppEvent::MultiAiCodeImGoal {
            request_id: payload.request_id?,
            goal: payload.goal,
        }),
        "btw" => Some(AppEvent::MultiAiCodeImBtw {
            request_id: payload.request_id?,
            task: payload.task.unwrap_or_default(),
            reply_id: payload.reply_id,
            task_id: payload.task_id,
        }),
        "submit_user_message" => {
            let remote_im_input = match payload.input_origin.as_deref() {
                Some("remote-im") => true,
                Some("local") => false,
                _ => payload.reply_id.is_some() || payload.task_id.is_some(),
            };
            Some(AppEvent::MultiAiCodeImSubmitUserMessage {
                request_id: payload.request_id?,
                text: payload.text.unwrap_or_default(),
                display_text: payload.display_text.unwrap_or_default(),
                local_image_paths: payload
                    .attachments
                    .into_iter()
                    .take(4)
                    .filter_map(|attachment| {
                        let path = PathBuf::from(attachment.local_path);
                        (attachment.attachment_type == "image"
                            && attachment.mime_type.starts_with("image/")
                            && path.is_absolute())
                        .then_some(path)
                    })
                    .collect(),
                remote_im_input,
                reply_id: payload.reply_id,
                task_id: payload.task_id,
            })
        }
        "interrupt" => Some(AppEvent::MultiAiCodeImInterrupt {
            request_id: payload.request_id?,
        }),
        "compact" => Some(AppEvent::MultiAiCodeImCompact {
            request_id: payload.request_id?,
        }),
        "clear" => Some(AppEvent::MultiAiCodeImClear {
            request_id: payload.request_id?,
        }),
        "resolve_approval" => Some(AppEvent::MultiAiCodeImResolveApproval {
            request_id: payload.request_id?,
            thread_id: payload.thread_id?,
            turn_id: payload.turn_id?,
            task_id: payload.task_id?,
            approval_id: payload.approval_id?,
            decision: payload.decision?,
        }),
        "theme" => {
            // bg 必填；fg 缺省时由 bg 的明暗推导（见 event_dispatch 处理）。
            let bg = parse_hex_rgb(payload.bg.as_deref()?)?;
            let fg = payload.fg.as_deref().and_then(parse_hex_rgb);
            Some(AppEvent::MultiAiCodeImTheme {
                request_id: payload.request_id,
                bg,
                fg,
            })
        }
        _ => None,
    }
}

pub(crate) fn send_task_started(reply_id: &str, task_id: Option<&str>) {
    send_reliable_text("task_started", "running", None, Some(reply_id), task_id);
}

pub(crate) fn send_task_activity(task_id: &str) {
    send_reliable_text("task_activity", "running", None, None, Some(task_id));
}

pub(crate) fn send_input_origin(remote_im: bool) {
    send_reliable_text(
        "input_origin",
        if remote_im { "remote-im" } else { "tui" },
        None,
        None,
        None,
    );
}

pub(crate) fn send_source_task_activity(reply_id: Option<&str>, task_id: Option<&str>) {
    send_reliable_text("task_activity", "running", None, reply_id, task_id);
}

pub(crate) fn send_source_assistant_text(
    text: &str,
    message_id: Option<&str>,
    reply_id: Option<&str>,
    task_id: Option<&str>,
) {
    send_reliable_text("assistant_text", text, message_id, reply_id, task_id);
}

pub(crate) fn send_source_assistant_final(
    text: &str,
    message_id: Option<&str>,
    reply_id: Option<&str>,
    task_id: Option<&str>,
) {
    if let Some(error) = empty_assistant_final_error(text) {
        send_source_turn_error(error, message_id, reply_id, task_id);
        return;
    }
    let message_id = message_id.map(|id| format!("{id}:final"));
    send_reliable_text(
        "assistant_final",
        text,
        message_id.as_deref(),
        reply_id,
        task_id,
    );
}

pub(crate) fn send_source_turn_error(
    text: &str,
    turn_id: Option<&str>,
    reply_id: Option<&str>,
    task_id: Option<&str>,
) {
    let message_id = turn_id.map(|id| format!("{id}:error"));
    send_reliable_text("turn_error", text, message_id.as_deref(), reply_id, task_id);
}

pub(crate) fn send_approval_request(
    thread_id: ThreadId,
    request: &ExecApprovalRequestEvent,
    reply_id: Option<&str>,
    task_id: Option<&str>,
) -> ReliableSendOutcome {
    let approval_id = request.effective_approval_id();
    let payload = approval_request_payload(thread_id, request, reply_id, task_id);
    send_security_reliable_payload(
        payload,
        format!("codex-approval-request:{thread_id}:{approval_id}"),
    )
}

fn approval_request_payload(
    thread_id: ThreadId,
    request: &ExecApprovalRequestEvent,
    reply_id: Option<&str>,
    task_id: Option<&str>,
) -> serde_json::Value {
    let approval_id = request.effective_approval_id();
    let command_text = request.raw_command.clone().unwrap_or_else(|| {
        shlex::try_join(request.command.iter().map(String::as_str))
            .unwrap_or_else(|_| request.command.join(" "))
    });
    let mut payload = json!({
        "kind": "approval_request",
        "approvalType": "command_execution",
        // Keep a text display value for host versions whose structured-output
        // ingress still requires text, while retaining the argv below for newer
        // clients that can render it without reparsing.
        "text": command_text,
        "approvalId": approval_id,
        "itemId": request.call_id,
        "threadId": thread_id.to_string(),
        "turnId": request.turn_id,
        "command": request.command,
        "cwd": request.cwd.display().to_string(),
        "reason": request.reason,
        "availableDecisions": request.effective_available_decisions(),
    });
    if let Some(reply_id) = reply_id {
        payload["replyId"] = reply_id.into();
    }
    if let Some(task_id) = task_id {
        payload["taskId"] = task_id.into();
    }
    payload
}

pub(crate) fn send_approval_resolved(
    thread_id: ThreadId,
    turn_id: &str,
    approval_id: &str,
    reply_id: Option<&str>,
    task_id: Option<&str>,
) {
    let mut payload = json!({
        "kind": "approval_resolved",
        "approvalType": "command_execution",
        "text": "resolved",
        "approvalId": approval_id,
        "threadId": thread_id.to_string(),
        "turnId": turn_id,
    });
    if let Some(reply_id) = reply_id {
        payload["replyId"] = reply_id.into();
    }
    if let Some(task_id) = task_id {
        payload["taskId"] = task_id.into();
    }
    send_reliable_payload(
        payload,
        format!("codex-approval-resolved:{thread_id}:{approval_id}"),
    );
}

pub(crate) fn send_assistant_text(text: &str, message_id: Option<&str>, task_id: Option<&str>) {
    send_reliable_text("assistant_text", text, message_id, None, task_id);
}

pub(crate) fn send_assistant_final(
    text: &str,
    message_id: Option<&str>,
    reply_id: &str,
    task_id: Option<&str>,
) {
    if let Some(error) = empty_assistant_final_error(text) {
        send_turn_error(error, message_id, reply_id, task_id);
        return;
    }
    let message_id = message_id.map(|id| format!("{id}:final"));
    send_reliable_text(
        "assistant_final",
        text,
        message_id.as_deref(),
        Some(reply_id),
        task_id,
    );
}

fn empty_assistant_final_error(text: &str) -> Option<&'static str> {
    text.trim()
        .is_empty()
        .then_some("Codex turn completed without a final assistant response.")
}

pub(crate) fn send_turn_error(
    text: &str,
    turn_id: Option<&str>,
    reply_id: &str,
    task_id: Option<&str>,
) {
    let message_id = turn_id.map(|id| format!("{id}:error"));
    send_reliable_text(
        "turn_error",
        text,
        message_id.as_deref(),
        Some(reply_id),
        task_id,
    );
}

fn send_reliable_text(
    kind: &'static str,
    text: &str,
    message_id: Option<&str>,
    reply_id: Option<&str>,
    task_id: Option<&str>,
) {
    if text.is_empty() {
        return;
    }
    let message_id = message_id
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(next_message_id);
    let mut payload = json!({
        "kind": kind,
        "text": text,
    });
    if let Some(reply_id) = reply_id {
        payload["replyId"] = reply_id.into();
    }
    if let Some(task_id) = task_id {
        payload["taskId"] = task_id.into();
    }
    send_reliable_payload(payload, message_id);
}

fn send_reliable_payload(payload: serde_json::Value, message_id: String) {
    let Some(sender) = data_sender() else {
        return;
    };
    let _ = sender.send(DataMsg::Reliable {
        priority: reliable_payload_priority(&payload),
        payload,
        message_id,
        admission: None,
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReliableSendOutcome {
    Queued,
    Unavailable,
    Saturated,
}

fn send_security_reliable_payload(
    payload: serde_json::Value,
    message_id: String,
) -> ReliableSendOutcome {
    let Some(sender) = data_sender() else {
        return ReliableSendOutcome::Unavailable;
    };
    let (admission_tx, admission_rx) = mpsc::sync_channel(1);
    if sender
        .send(DataMsg::Reliable {
            priority: ReliablePriority::SecurityApproval,
            payload,
            message_id,
            admission: Some(admission_tx),
        })
        .is_err()
    {
        return ReliableSendOutcome::Unavailable;
    }
    match admission_rx.recv_timeout(std::time::Duration::from_secs(1)) {
        Ok(true) => ReliableSendOutcome::Queued,
        Ok(false) => ReliableSendOutcome::Saturated,
        // The manager may be delayed behind a large channel backlog. Treat a
        // timed-out admission as rejected; the receiver is dropped, so the
        // manager will later observe send(true) failure and must not deliver it.
        Err(_) => ReliableSendOutcome::Saturated,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReliablePriority {
    Normal,
    SecurityApproval,
}

fn reliable_payload_priority(payload: &serde_json::Value) -> ReliablePriority {
    match payload.get("kind").and_then(serde_json::Value::as_str) {
        Some("approval_request" | "approval_resolved") => ReliablePriority::SecurityApproval,
        _ => ReliablePriority::Normal,
    }
}

pub(crate) fn send_control_result(request_id: &str, ok: bool, text: &str, error: Option<&str>) {
    let Some(config) = BRIDGE_CONFIG.get().and_then(|item| item.as_ref()) else {
        return;
    };
    let Some(sender) = data_sender() else {
        return;
    };
    let payload = json!({
        "token": config.token,
        "kind": "control_result",
        "requestId": request_id,
        "ok": ok,
        "text": text,
        "error": error,
    });
    // control_result is an RPC response guarded by the host's request timeout, so
    // it does not need per-message ack tracking; it simply rides the same
    // self-healing data connection.
    let _ = sender.send(DataMsg::ControlResult {
        line: format!("{payload}\n"),
    });
}

fn next_message_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("codex-im-{seq}")
}

enum DataMsg {
    Reliable {
        payload: serde_json::Value,
        message_id: String,
        priority: ReliablePriority,
        admission: Option<mpsc::SyncSender<bool>>,
    },
    ControlResult {
        line: String,
    },
    // Funneled in from the reader sub-thread:
    Ack {
        message_id: String,
    },
    PeerClosed {
        generation: u64,
    },
}

struct PendingReliableMessage {
    message_id: String,
    line: String,
    sent_at: Instant,
    priority: ReliablePriority,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReliableAdmission {
    Append,
    ReplaceNormal(usize),
    Reject,
}

fn reliable_message_admission(
    pending: &VecDeque<PendingReliableMessage>,
    incoming: ReliablePriority,
) -> ReliableAdmission {
    if pending.len() < MAX_PENDING {
        return ReliableAdmission::Append;
    }
    if let Some(index) = pending
        .iter()
        .position(|item| item.priority == ReliablePriority::Normal)
    {
        return ReliableAdmission::ReplaceNormal(index);
    }
    // Never silently evict a security approval with another event. If every
    // slot is already security-critical, retain the existing requests and make
    // the saturation explicit. The caller logs the refused incoming event.
    let _ = incoming;
    ReliableAdmission::Reject
}

fn confirm_reliable_admission(admission: Option<mpsc::SyncSender<bool>>, accepted: bool) -> bool {
    admission.is_none_or(|admission| admission.send(accepted).is_ok())
}

#[derive(Deserialize)]
struct AckPayload {
    token: Option<String>,
    kind: Option<String>,
    #[serde(rename = "messageId")]
    message_id: Option<String>,
}

fn data_sender() -> Option<&'static mpsc::Sender<DataMsg>> {
    DATA_SENDER
        .get_or_init(|| {
            let config = BRIDGE_CONFIG
                .get()
                .and_then(|item| item.as_ref())
                .cloned()?;
            let (tx, rx) = mpsc::channel::<DataMsg>();
            let tx_for_manager = tx.clone();
            thread::spawn(move || run_data_manager(config, rx, tx_for_manager));
            Some(tx)
        })
        .as_ref()
}

// Single-owner manager: it is the only thread that touches the write stream and
// the pending queue, so there is no lock held across blocking I/O. Acks and
// peer-close events arrive as messages from the reader sub-thread, keeping the
// state machine sequential and race-free.
fn run_data_manager(config: BridgeConfig, rx: mpsc::Receiver<DataMsg>, tx: mpsc::Sender<DataMsg>) {
    let mut stream: Option<TcpStream> = None;
    let mut generation: u64 = 0;
    let mut pending: VecDeque<PendingReliableMessage> = VecDeque::new();

    loop {
        match rx.recv_timeout(WATCHDOG_TICK) {
            Ok(DataMsg::Reliable {
                mut payload,
                message_id,
                priority,
                admission,
            }) => {
                payload["token"] = config.token.clone().into();
                payload["messageId"] = message_id.clone().into();
                let line = format!("{payload}\n");
                let admission_plan = reliable_message_admission(&pending, priority);
                if admission_plan == ReliableAdmission::Reject {
                    tracing::error!(
                        %message_id,
                        ?priority,
                        "remote IM reliable queue is saturated with security approvals; refusing to evict an approval event"
                    );
                    confirm_reliable_admission(admission, /*accepted*/ false);
                    continue;
                }
                // Confirm synchronous security admission before mutating the
                // queue or writing to the host. If the caller timed out and
                // already canceled fail-closed, this send fails and prevents a
                // stale approval notification from being delivered later.
                if !confirm_reliable_admission(admission, /*accepted*/ true) {
                    continue;
                }
                if let ReliableAdmission::ReplaceNormal(index) = admission_plan {
                    pending.remove(index);
                }
                pending.push_back(PendingReliableMessage {
                    message_id,
                    line: line.clone(),
                    sent_at: Instant::now(),
                    priority,
                });
                write_line(&config, &mut stream, &mut generation, &tx, &line);
            }
            Ok(DataMsg::ControlResult { line }) => {
                write_line(&config, &mut stream, &mut generation, &tx, &line);
            }
            Ok(DataMsg::Ack { message_id }) => {
                if let Some(pos) = pending
                    .iter()
                    .position(|item| item.message_id == message_id)
                {
                    pending.remove(pos);
                }
            }
            Ok(DataMsg::PeerClosed { generation: closed }) => {
                if closed == generation
                    && let Some(old) = stream.take()
                {
                    let _ = old.shutdown(Shutdown::Both);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let stale = pending
                    .front()
                    .is_some_and(|item| item.sent_at.elapsed() >= ACK_TIMEOUT);
                if stale {
                    if let Some(old) = stream.take() {
                        let _ = old.shutdown(Shutdown::Both);
                    }
                    let resend: Vec<String> = pending
                        .iter()
                        .take(MAX_RESEND)
                        .map(|item| item.line.clone())
                        .collect();
                    let now = Instant::now();
                    for line in &resend {
                        write_line(&config, &mut stream, &mut generation, &tx, line);
                    }
                    for item in pending.iter_mut().take(MAX_RESEND) {
                        item.sent_at = now;
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn write_line(
    config: &BridgeConfig,
    stream: &mut Option<TcpStream>,
    generation: &mut u64,
    tx: &mpsc::Sender<DataMsg>,
    line: &str,
) {
    if stream.is_none() {
        let Ok(new_stream) = TcpStream::connect((config.host.as_str(), config.port)) else {
            // Stay disconnected; the watchdog / next send will retry, and pending
            // reliable events are preserved for resend.
            return;
        };
        *generation += 1;
        let current_gen = *generation;
        if let Ok(reader_stream) = new_stream.try_clone() {
            let token = config.token.clone();
            let tx_reader = tx.clone();
            thread::spawn(move || run_data_reader(reader_stream, token, current_gen, tx_reader));
        }
        *stream = Some(new_stream);
    }
    if let Some(writer) = stream.as_mut()
        && writer.write_all(line.as_bytes()).is_err()
        && let Some(old) = stream.take()
    {
        let _ = old.shutdown(Shutdown::Both);
    }
}

fn run_data_reader(stream: TcpStream, token: String, generation: u64, tx: mpsc::Sender<DataMsg>) {
    let reader = BufReader::new(stream);
    for line in reader.lines().map_while(Result::ok) {
        let Ok(payload) = serde_json::from_str::<AckPayload>(&line) else {
            continue;
        };
        if payload.token.as_deref() != Some(token.as_str()) {
            continue;
        }
        if payload.kind.as_deref() == Some("ack")
            && let Some(message_id) = payload.message_id
        {
            let _ = tx.send(DataMsg::Ack { message_id });
        }
    }
    // EOF / error: report so the manager drops this generation's stream and the
    // next write reconnects (also covers the host closing the data socket).
    let _ = tx.send(DataMsg::PeerClosed { generation });
}

fn parse_endpoint(endpoint: &str) -> Option<BridgeConfig> {
    let url = Url::parse(endpoint).ok()?;
    if url.scheme() != "tcp" {
        return None;
    }
    Some(BridgeConfig {
        host: url.host_str()?.to_string(),
        port: url.port()?,
        token: url.query_pairs().find_map(|(key, value)| {
            (key == "token" && !value.is_empty()).then(|| value.into_owned())
        })?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control_payload(command: &str) -> ControlPayload {
        ControlPayload {
            token: Some("token".to_string()),
            kind: Some("control".to_string()),
            command: Some(command.to_string()),
            mode: None,
            model: None,
            reasoning: None,
            goal: None,
            task: None,
            text: None,
            display_text: None,
            input_origin: None,
            attachments: Vec::new(),
            bg: None,
            fg: None,
            reply_id: None,
            task_id: None,
            thread_id: None,
            turn_id: None,
            approval_id: None,
            decision: None,
            request_id: Some("req-1".to_string()),
        }
    }

    #[test]
    fn lifecycle_control_payloads_map_to_app_events() {
        for command in ["interrupt", "compact", "clear"] {
            let event = control_payload_to_app_event(control_payload(command), "token")
                .expect("expected lifecycle command to map to an app event");
            match command {
                "interrupt" => assert!(
                    matches!(event, AppEvent::MultiAiCodeImInterrupt { request_id } if request_id == "req-1")
                ),
                "compact" => assert!(
                    matches!(event, AppEvent::MultiAiCodeImCompact { request_id } if request_id == "req-1")
                ),
                "clear" => assert!(
                    matches!(event, AppEvent::MultiAiCodeImClear { request_id } if request_id == "req-1")
                ),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn resolve_approval_control_preserves_security_identity() {
        for decision_value in ["accept", "cancel", "invalid"] {
            let mut payload = control_payload("resolve_approval");
            payload.thread_id = Some("00000000-0000-0000-0000-000000000123".to_string());
            payload.turn_id = Some("turn-1".to_string());
            payload.task_id = Some("task-1".to_string());
            payload.approval_id = Some("approval-1".to_string());
            payload.decision = Some(decision_value.to_string());

            let event = control_payload_to_app_event(payload, "token")
                .expect("well-formed approval response should reach app-level validation");

            assert!(matches!(
                event,
                AppEvent::MultiAiCodeImResolveApproval {
                    request_id,
                    thread_id,
                    turn_id,
                    task_id,
                    approval_id,
                    decision,
                } if request_id == "req-1"
                    && thread_id == "00000000-0000-0000-0000-000000000123"
                    && turn_id == "turn-1"
                    && task_id == "task-1"
                    && approval_id == "approval-1"
                    && decision == decision_value
            ));
        }

        for missing in ["thread", "turn", "task", "approval", "decision"] {
            let mut payload = control_payload("resolve_approval");
            payload.thread_id =
                (missing != "thread").then(|| "00000000-0000-0000-0000-000000000123".to_string());
            payload.turn_id = (missing != "turn").then(|| "turn-1".to_string());
            payload.task_id = (missing != "task").then(|| "task-1".to_string());
            payload.approval_id = (missing != "approval").then(|| "approval-1".to_string());
            payload.decision = (missing != "decision").then(|| "accept".to_string());
            assert!(
                control_payload_to_app_event(payload, "token").is_none(),
                "missing {missing} must not produce an approval response"
            );
        }
    }

    #[test]
    fn approval_request_payload_is_structured_and_explicitly_task_routed() {
        let thread_id = ThreadId::new();
        let request = ExecApprovalRequestEvent {
            call_id: "item-1".to_string(),
            approval_id: Some("approval-1".to_string()),
            turn_id: "turn-1".to_string(),
            environment_id: None,
            raw_command: Some(
                "powershell  -Command  \"Remove-Item -Recurse -Force 'C:\\\\tmp\\\\target file'\""
                    .to_string(),
            ),
            command: vec![
                "powershell".to_string(),
                "-Command".to_string(),
                "Remove-Item -Recurse -Force C:\\tmp\\target".to_string(),
            ],
            cwd: codex_utils_absolute_path::AbsolutePathBuf::current_dir()
                .expect("current directory"),
            reason: Some("dangerous command".to_string()),
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            available_decisions: Some(vec![
                codex_app_server_protocol::CommandExecutionApprovalDecision::Accept,
                codex_app_server_protocol::CommandExecutionApprovalDecision::Cancel,
            ]),
            network_approval_context: None,
            additional_permissions: None,
        };

        let payload =
            approval_request_payload(thread_id, &request, Some("reply-1"), Some("task-1"));

        assert_eq!(payload["kind"], "approval_request");
        assert_eq!(payload["approvalId"], "approval-1");
        assert_eq!(payload["itemId"], "item-1");
        assert_eq!(payload["threadId"], thread_id.to_string());
        assert_eq!(payload["turnId"], "turn-1");
        assert_eq!(payload["replyId"], "reply-1");
        assert_eq!(payload["taskId"], "task-1");
        assert_eq!(
            payload["text"],
            "powershell  -Command  \"Remove-Item -Recurse -Force 'C:\\\\tmp\\\\target file'\""
        );
        assert_eq!(payload["availableDecisions"], json!(["accept", "cancel"]));
        assert!(
            payload["text"]
                .as_str()
                .is_some_and(|text| text.contains("Remove-Item"))
        );
        assert_eq!(payload["command"], json!(request.command));
    }

    fn pending_message(priority: ReliablePriority, index: usize) -> PendingReliableMessage {
        PendingReliableMessage {
            message_id: format!("message-{index}"),
            line: format!("line-{index}"),
            sent_at: Instant::now(),
            priority,
        }
    }

    #[test]
    fn security_approval_replaces_normal_output_but_never_another_approval() {
        let mut normal_queue = (0..MAX_PENDING)
            .map(|index| pending_message(ReliablePriority::Normal, index))
            .collect::<VecDeque<_>>();
        assert!(matches!(
            reliable_message_admission(&normal_queue, ReliablePriority::SecurityApproval),
            ReliableAdmission::ReplaceNormal(0)
        ));
        normal_queue[0].priority = ReliablePriority::SecurityApproval;
        assert!(matches!(
            reliable_message_admission(&normal_queue, ReliablePriority::SecurityApproval),
            ReliableAdmission::ReplaceNormal(1)
        ));

        let approval_queue = (0..MAX_PENDING)
            .map(|index| pending_message(ReliablePriority::SecurityApproval, index))
            .collect::<VecDeque<_>>();
        assert_eq!(
            reliable_message_admission(&approval_queue, ReliablePriority::SecurityApproval),
            ReliableAdmission::Reject
        );
        assert_eq!(
            reliable_message_admission(&approval_queue, ReliablePriority::Normal),
            ReliableAdmission::Reject
        );
    }

    #[test]
    fn dropped_security_admission_prevents_late_delivery() {
        let (admission_tx, admission_rx) = mpsc::sync_channel(1);
        drop(admission_rx);
        assert!(!confirm_reliable_admission(
            Some(admission_tx),
            /*accepted*/ true
        ));
    }

    #[test]
    fn btw_control_payload_preserves_reply_id() {
        let mut payload = control_payload("btw");
        payload.task = Some("检查日志".to_string());
        payload.reply_id = Some("reply-btw-fixed".to_string());
        payload.task_id = Some("task-btw-fixed".to_string());

        let event = control_payload_to_app_event(payload, "token")
            .expect("expected /btw control payload to map to app event");

        assert!(matches!(
            event,
            AppEvent::MultiAiCodeImBtw {
                request_id,
                task,
                reply_id,
                task_id
            } if request_id == "req-1"
                && task == "检查日志"
                && reply_id.as_deref() == Some("reply-btw-fixed")
                && task_id.as_deref() == Some("task-btw-fixed")
        ));
    }

    #[test]
    fn parses_remote_im_side_task_id_without_accepting_unsafe_marker_text() {
        assert_eq!(
            remote_im_task_id("Task marker: <remote-im-task id=\"remote-im-task-0123_abcd\">")
                .as_deref(),
            Some("remote-im-task-0123_abcd")
        );
        assert_eq!(
            remote_im_task_id("Task marker: <remote-im-task id=\"task with spaces\">"),
            None
        );
    }

    #[test]
    fn submit_user_message_payload_preserves_model_and_display_text() {
        let mut payload = control_payload("submit_user_message");
        payload.text = Some("wrapped model prompt".to_string());
        payload.display_text = Some("[来自远程 IM：phone]\n你好".to_string());
        payload.input_origin = Some("remote-im".to_string());
        let image_path = std::env::current_dir()
            .expect("current directory")
            .join("task-image.png");
        payload.attachments = vec![ControlAttachment {
            attachment_type: "image".to_string(),
            local_path: image_path.to_string_lossy().into_owned(),
            mime_type: "image/png".to_string(),
        }];
        payload.reply_id = Some("rim-fixed".to_string());
        payload.task_id = Some("task-fixed".to_string());

        let event = control_payload_to_app_event(payload, "token")
            .expect("expected ordinary IM message to map to app event");

        assert!(matches!(
            event,
            AppEvent::MultiAiCodeImSubmitUserMessage {
                request_id,
                text,
                display_text,
                local_image_paths,
                remote_im_input,
                reply_id,
                task_id
            } if request_id == "req-1"
                && text == "wrapped model prompt"
                && display_text == "[来自远程 IM：phone]\n你好"
                && local_image_paths == vec![image_path]
                && remote_im_input
                && reply_id.as_deref() == Some("rim-fixed")
                && task_id.as_deref() == Some("task-fixed")
        ));
    }

    #[test]
    fn empty_assistant_final_is_classified_as_a_terminal_error() {
        assert_eq!(
            empty_assistant_final_error(" \n\t"),
            Some("Codex turn completed without a final assistant response.")
        );
        assert_eq!(empty_assistant_final_error("completed"), None);
    }

    #[test]
    fn extracts_remote_im_reply_id_only_from_the_prompt_instruction() {
        assert_eq!(
            remote_im_reply_id(
                "[IM_REPLY]\nOpening marker: <remote-im-reply id=\"rim-0123456789abcdef\">"
            )
            .as_deref(),
            Some("rim-0123456789abcdef")
        );
        assert_eq!(
            remote_im_reply_id("<remote-im-reply id=\"rim-0123456789abcdef\">answer"),
            None
        );
    }

    #[test]
    fn remote_im_reply_filter_hides_markers_and_streams_visible_markdown() {
        let mut filter = RemoteImReplyDisplayFilter::default();
        assert_eq!(filter.push("<remote-im-re"), "");
        assert_eq!(filter.push("ply id=\"rim-1\"># 回复\n"), "# 回复\n");
        assert_eq!(filter.push("内容\n</remote-im-re"), "内容\n");
        assert_eq!(filter.push("ply id=\"rim-1\">"), "");
        assert_eq!(
            visible_remote_im_reply_text(
                "<remote-im-reply id=\"rim-1\">\n# 回复\n内容\n</remote-im-reply id=\"rim-1\">"
            ),
            "# 回复\n内容\n"
        );
        assert_eq!(
            visible_remote_im_reply_text(
                "<remote-im-reply id=\"rim-0123456789abcdef你好，有什么需要我帮忙的？\n</remote-im-reply id=\"rim-0123456789abcdef"
            ),
            "你好，有什么需要我帮忙的？\n"
        );
        assert_eq!(visible_remote_im_reply_text("普通本地回复"), "普通本地回复");
    }
}
