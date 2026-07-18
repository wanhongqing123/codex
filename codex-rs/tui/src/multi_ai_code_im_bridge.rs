use serde::Deserialize;
use serde_json::json;
use std::collections::VecDeque;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::net::Shutdown;
use std::net::TcpStream;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use url::Url;

use codex_protocol::config_types::ModeKind;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;

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
// control_result) is funneled through this so a single owner thread controls the
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
    // 运行时主题：宿主终端的背景/前景色（6 位十六进制，可带 #）。
    bg: Option<String>,
    fg: Option<String>,
    #[serde(rename = "replyId")]
    reply_id: Option<String>,
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
        }),
        "submit_user_message" => Some(AppEvent::MultiAiCodeImSubmitUserMessage {
            request_id: payload.request_id?,
            text: payload.text.unwrap_or_default(),
            display_text: payload.display_text.unwrap_or_default(),
        }),
        "interrupt" => Some(AppEvent::MultiAiCodeImInterrupt {
            request_id: payload.request_id?,
        }),
        "compact" => Some(AppEvent::MultiAiCodeImCompact {
            request_id: payload.request_id?,
        }),
        "clear" => Some(AppEvent::MultiAiCodeImClear {
            request_id: payload.request_id?,
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

pub(crate) fn send_assistant_text(text: &str, message_id: Option<&str>) {
    if text.is_empty() {
        return;
    }
    let Some(sender) = data_sender() else {
        return;
    };
    let message_id = message_id
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(next_message_id);
    let _ = sender.send(DataMsg::AssistantText {
        text: text.to_string(),
        message_id,
    });
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
    AssistantText { text: String, message_id: String },
    ControlResult { line: String },
    // Funneled in from the reader sub-thread:
    Ack { message_id: String },
    PeerClosed { generation: u64 },
}

struct PendingAssistantText {
    message_id: String,
    line: String,
    sent_at: Instant,
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
    let mut pending: VecDeque<PendingAssistantText> = VecDeque::new();

    loop {
        match rx.recv_timeout(WATCHDOG_TICK) {
            Ok(DataMsg::AssistantText { text, message_id }) => {
                let payload = json!({
                    "token": config.token,
                    "kind": "assistant_text",
                    "text": text,
                    "messageId": message_id.clone(),
                });
                let line = format!("{payload}\n");
                if pending.len() >= MAX_PENDING {
                    pending.pop_front();
                }
                pending.push_back(PendingAssistantText {
                    message_id,
                    line: line.clone(),
                    sent_at: Instant::now(),
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
            // assistant_text is preserved for resend.
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
    {
        if let Some(old) = stream.take() {
            let _ = old.shutdown(Shutdown::Both);
        }
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
            bg: None,
            fg: None,
            reply_id: None,
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
    fn btw_control_payload_preserves_reply_id() {
        let mut payload = control_payload("btw");
        payload.task = Some("检查日志".to_string());
        payload.reply_id = Some("reply-btw-fixed".to_string());

        let event = control_payload_to_app_event(payload, "token")
            .expect("expected /btw control payload to map to app event");

        assert!(matches!(
            event,
            AppEvent::MultiAiCodeImBtw {
                request_id,
                task,
                reply_id
            } if request_id == "req-1"
                && task == "检查日志"
                && reply_id.as_deref() == Some("reply-btw-fixed")
        ));
    }

    #[test]
    fn submit_user_message_payload_preserves_model_and_display_text() {
        let mut payload = control_payload("submit_user_message");
        payload.text = Some("wrapped model prompt".to_string());
        payload.display_text = Some("[来自远程 IM：phone]\n你好".to_string());

        let event = control_payload_to_app_event(payload, "token")
            .expect("expected ordinary IM message to map to app event");

        assert!(matches!(
            event,
            AppEvent::MultiAiCodeImSubmitUserMessage {
                request_id,
                text,
                display_text
            } if request_id == "req-1"
                && text == "wrapped model prompt"
                && display_text == "[来自远程 IM：phone]\n你好"
        ));
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
