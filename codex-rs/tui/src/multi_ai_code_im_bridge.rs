use serde_json::json;
use std::io::Write;
use std::net::TcpStream;
use std::sync::Mutex;
use std::sync::OnceLock;
use url::Url;

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
