use serde::Deserialize;
use serde_json::json;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::net::TcpStream;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::thread;
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

static BRIDGE_CONFIG: OnceLock<Option<BridgeConfig>> = OnceLock::new();
static BRIDGE_STREAM: OnceLock<Mutex<Option<TcpStream>>> = OnceLock::new();

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
    #[serde(rename = "requestId")]
    request_id: Option<String>,
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
                if payload.token.as_deref() != Some(config.token.as_str())
                    || payload.kind.as_deref() != Some("control")
                {
                    continue;
                }
                match payload.command.as_deref() {
                    Some("switch_mode") => {
                        let mode = match payload.mode.as_deref() {
                            Some("plan") => ModeKind::Plan,
                            Some("build") => ModeKind::Default,
                            _ => continue,
                        };
                        app_event_tx.send(AppEvent::MultiAiCodeImSwitchMode {
                            mode,
                            request_id: payload.request_id,
                        });
                    }
                    Some("status") => {
                        let Some(request_id) = payload.request_id else {
                            continue;
                        };
                        app_event_tx.send(AppEvent::MultiAiCodeImStatus { request_id });
                    }
                    Some("model") => {
                        let Some(request_id) = payload.request_id else {
                            continue;
                        };
                        app_event_tx.send(AppEvent::MultiAiCodeImModel {
                            request_id,
                            model: payload.model,
                        });
                    }
                    _ => {}
                }
            }
        }
    });
}

pub(crate) fn send_assistant_text(text: &str, message_id: Option<&str>) {
    if text.is_empty() {
        return;
    }
    let Some(config) = BRIDGE_CONFIG.get().and_then(|item| item.as_ref()) else {
        return;
    };
    let stream_lock = BRIDGE_STREAM.get_or_init(|| Mutex::new(None));
    let Ok(mut stream) = stream_lock.lock() else {
        return;
    };

    if stream.is_none() {
        *stream = TcpStream::connect((config.host.as_str(), config.port)).ok();
    }

    let payload = json!({
        "token": config.token,
        "kind": "assistant_text",
        "text": text,
        "messageId": message_id,
    });
    let line = format!("{payload}\n");

    if let Some(writer) = stream.as_mut()
        && writer.write_all(line.as_bytes()).is_ok()
    {
        return;
    }

    *stream = None;
}

pub(crate) fn send_control_result(request_id: &str, ok: bool, text: &str, error: Option<&str>) {
    let Some(config) = BRIDGE_CONFIG.get().and_then(|item| item.as_ref()) else {
        return;
    };
    let stream_lock = BRIDGE_STREAM.get_or_init(|| Mutex::new(None));
    let Ok(mut stream) = stream_lock.lock() else {
        return;
    };

    if stream.is_none() {
        *stream = TcpStream::connect((config.host.as_str(), config.port)).ok();
    }

    let payload = json!({
        "token": config.token,
        "kind": "control_result",
        "requestId": request_id,
        "ok": ok,
        "text": text,
        "error": error,
    });
    let line = format!("{payload}\n");

    if let Some(writer) = stream.as_mut()
        && writer.write_all(line.as_bytes()).is_ok()
    {
        return;
    }

    *stream = None;
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
