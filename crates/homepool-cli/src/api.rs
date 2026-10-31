//! Read-only HTTP facade. Authentication and routing precede every hub call.
use crate::{hub, private, protocol::Request, settings::*};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
use subtle::ConstantTimeEq;

struct Token {
    bytes: [u8; HTTP_TOKEN_BYTES],
    len: usize,
}
impl Token {
    fn parse(bytes: &[u8]) -> Option<Self> {
        let text = bytes.split(|&b| b == b'=').next()?;
        if bytes.is_empty()
            || bytes.len() > HTTP_TOKEN_BYTES
            || text.is_empty()
            || !text
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~+/".contains(b))
            || !bytes[text.len()..].iter().all(|&b| b == b'=')
        {
            return None;
        }
        let mut token = Self {
            bytes: [0; HTTP_TOKEN_BYTES],
            len: bytes.len(),
        };
        token.bytes[..bytes.len()].copy_from_slice(bytes);
        Some(token)
    }
    fn load(path: &Path) -> Result<Self, String> {
        let load = || {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            private::check(parent, true).ok()?;
            let before = private::check(path, false).ok()?;
            if !before.is_file() {
                return None;
            }
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(
                    (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
                )
                .open(path)
                .ok()?;
            let after = file.metadata().ok()?;
            if before.ino() != after.ino() || before.dev() != after.dev() {
                return None;
            }
            let mut bytes = Vec::new();
            file.take((HTTP_TOKEN_BYTES + 3) as u64)
                .read_to_end(&mut bytes)
                .ok()?;
            let bytes = bytes
                .strip_suffix(b"\r\n")
                .or_else(|| bytes.strip_suffix(b"\n"))
                .unwrap_or(&bytes);
            Self::parse(bytes)
        };
        load().ok_or_else(|| "invalid API token file".into())
    }
    fn matches(&self, bytes: &[u8]) -> bool {
        let Some(candidate) = Self::parse(bytes) else {
            return false;
        };
        bool::from(self.bytes.ct_eq(&candidate.bytes) & self.len.ct_eq(&candidate.len))
    }
}

pub fn serve(listen: SocketAddr, token_file: &Path, socket: &Path) -> Result<(), String> {
    if !listen.ip().is_loopback() {
        return Err("API listen address must be loopback".into());
    }
    let token = Arc::new(Token::load(token_file)?);
    let timing = Timing::load()?;
    let listener = TcpListener::bind(listen).map_err(|_| "could not bind API listener")?;
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("API accept failed".into()),
        };
        // No unbounded thread or work queue. Excess connections are closed.
        if active.load(Ordering::Acquire) >= HTTP_CONNECTIONS {
            continue;
        }
        active.fetch_add(1, Ordering::AcqRel);
        let slot = ConnectionSlot(Arc::clone(&active));
        let token = Arc::clone(&token);
        let socket = socket.to_owned();
        let accepted = Instant::now();
        std::thread::Builder::new()
            .spawn(move || {
                let _slot = slot;
                handle(stream, &token, &socket, accepted, timing);
            })
            .map_err(|_| "could not start API connection worker")?;
    }
    Ok(())
}
struct ConnectionSlot(Arc<AtomicUsize>);
impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn handle(mut stream: TcpStream, token: &Token, socket: &Path, accepted: Instant, timing: Timing) {
    let header_end = accepted + HTTP_IO_TIMEOUT;
    let result =
        read_request(&mut stream, header_end).and_then(|bytes| route(&bytes, token, socket));
    let (status, body) = result.unwrap_or_else(|status| (status, Vec::new()));
    // Hub calls have their existing total request budget. Slow reads and writes
    // cannot extend this absolute connection ceiling by making tiny progress.
    let Some(end) = header_end
        .checked_add(timing.request)
        .and_then(|t| t.checked_add(HTTP_IO_TIMEOUT))
    else {
        return;
    };
    let end = end.min(Instant::now() + HTTP_IO_TIMEOUT);
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        431 => "Request Header Fields Too Large",
        _ => "Service Unavailable",
    };
    let extra = match status {
        401 => "WWW-Authenticate: Bearer\r\n",
        405 => "Allow: GET\r\n",
        _ => "",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n{extra}\r\n",
        body.len()
    );
    let mut response = header.into_bytes();
    response.extend(body);
    let mut remaining = response.as_slice();
    while !remaining.is_empty() {
        let wait = end.saturating_duration_since(Instant::now());
        if wait.is_zero() || stream.set_write_timeout(Some(wait)).is_err() {
            break;
        }
        match stream.write(remaining) {
            Ok(0) | Err(_) => break,
            Ok(n) => remaining = &remaining[n..],
        }
    }
}

fn read_request(stream: &mut TcpStream, end: Instant) -> Result<Vec<u8>, u16> {
    let mut bytes = Vec::new();
    loop {
        let wait = end.saturating_duration_since(Instant::now());
        if wait.is_zero() {
            return Err(408);
        }
        stream.set_read_timeout(Some(wait)).map_err(|_| 400u16)?;
        let mut buffer = [0; 1024];
        let n = stream.read(&mut buffer).map_err(|_| 408u16)?;
        if n == 0 {
            return Err(400);
        }
        bytes.extend_from_slice(&buffer[..n]);
        if bytes.len() > HTTP_HEADER_BYTES {
            return Err(431);
        }
        if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(bytes);
        }
    }
}

fn route(bytes: &[u8], token: &Token, socket: &Path) -> Result<(u16, Vec<u8>), u16> {
    let mut headers = [httparse::EMPTY_HEADER; HTTP_HEADER_COUNT];
    let mut request = httparse::Request::new(&mut headers);
    match request.parse(bytes) {
        Ok(httparse::Status::Complete(_)) => {}
        Err(httparse::Error::TooManyHeaders) => return Err(431),
        _ => return Err(400),
    }
    let path = request.path.ok_or(400u16)?;
    if !["/v1/health", "/v1/sandboxes"].contains(&path) {
        return Err(404);
    }
    if request.method != Some("GET") {
        return Err(405);
    }
    if path == "/v1/health" {
        return Ok((200, br#"{"version":1}"#.to_vec()));
    }
    let mut auth = request
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("authorization"));
    let header = auth.next().ok_or(401u16)?;
    if auth.next().is_some() {
        return Err(401);
    }
    let separator = header.value.iter().position(|&b| b == b' ').ok_or(401u16)?;
    let (scheme, rest) = header.value.split_at(separator);
    let supplied = &rest[1..];
    if !scheme.eq_ignore_ascii_case(b"Bearer") || !token.matches(supplied) {
        return Err(401);
    }
    // This slice accepts no request bodies or transfer framing.
    if request.headers.iter().any(|h| {
        h.name.eq_ignore_ascii_case("transfer-encoding")
            || (h.name.eq_ignore_ascii_case("content-length") && h.value != b"0")
    }) {
        return Err(400);
    }
    let value = hub::call(socket, Request::Inventory {}).map_err(|_| 503u16)?;
    encode_inventory(value).map(|body| (200, body))
}

fn encode_inventory(value: serde_json::Value) -> Result<Vec<u8>, u16> {
    let inventory: HubInventory = serde_json::from_value(value).map_err(|_| 503u16)?;
    let inventory_complete =
        !inventory.settling && inventory.members.iter().all(|m| m.inventory_complete);
    let sandboxes = inventory
        .members
        .into_iter()
        .flat_map(|m| {
            m.sandboxes.into_iter().map(move |s| Sandbox {
                member: m.member,
                sandbox_id: s.sandbox_id,
                state: s.state,
                exit_code: s.exit_code,
            })
        })
        .collect();
    serde_json::to_vec(&Sandboxes {
        sandboxes,
        inventory_complete,
    })
    .map_err(|_| 503)
}

// Decode the complete known schema, then construct a separate public response.
// Additive hub fields fail closed until explicitly reviewed here.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HubInventory {
    settling: bool,
    members: Vec<HubMember>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HubMember {
    member: u64,
    #[serde(rename = "state")]
    _state: String,
    inventory_complete: bool,
    #[serde(rename = "free")]
    _free: HubBudget,
    sandboxes: Vec<HubSandbox>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HubBudget {
    #[serde(rename = "cpu_millicores")]
    _cpu: u64,
    #[serde(rename = "memory_bytes")]
    _memory: u64,
    #[serde(rename = "sandbox_slots")]
    _slots: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HubSandbox {
    #[serde(rename = "handle")]
    _handle: String,
    sandbox_id: String,
    state: String,
    exit_code: Option<i64>,
    #[serde(rename = "cpu_millicores")]
    _cpu: Option<u64>,
    #[serde(rename = "memory_bytes")]
    _memory: Option<u64>,
}
#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct Sandboxes {
    sandboxes: Vec<Sandbox>,
    inventory_complete: bool,
}
#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct Sandbox {
    member: u64,
    sandbox_id: String,
    state: String,
    exit_code: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inventory_projection_fails_closed_and_marks_stale_observations() {
        let source = json!({"settling":false,"members":[{"member":1,"state":"active","inventory_complete":true,"free":{"cpu_millicores":10,"memory_bytes":20,"sandbox_slots":1},"sandboxes":[{"handle":"1:example","sandbox_id":"example","state":"exited","exit_code":7,"cpu_millicores":10,"memory_bytes":20}]}]});
        let public: serde_json::Value =
            serde_json::from_slice(&encode_inventory(source.clone()).unwrap()).unwrap();
        assert_eq!(
            public,
            json!({"inventory_complete":true,"sandboxes":[{"member":1,"sandbox_id":"example","state":"exited","exit_code":7}]})
        );
        let mut incomplete = source.clone();
        incomplete["members"][0]["inventory_complete"] = json!(false);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&encode_inventory(incomplete).unwrap())
                .unwrap()["inventory_complete"],
            false
        );
        let mut settling = source.clone();
        settling["settling"] = json!(true);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&encode_inventory(settling).unwrap())
                .unwrap()["inventory_complete"],
            false
        );
        for pointer in [
            "",
            "/members/0",
            "/members/0/free",
            "/members/0/sandboxes/0",
        ] {
            let mut extra = source.clone();
            extra.pointer_mut(pointer).unwrap()["private"] = json!("must never pass through");
            assert_eq!(encode_inventory(extra).unwrap_err(), 503);
        }
    }

    #[test]
    fn token_grammar_and_fixed_width_comparison() {
        let token = Token::parse(b"example+/==").unwrap();
        assert!(token.matches(b"example+/=="));
        for wrong in [
            b"Example+/==".as_slice(),
            b"example+/=",
            b"example+/===",
            b"example+/==x",
            b"example +/==",
            b"",
            b"=",
        ] {
            assert!(!token.matches(wrong));
        }
    }
}
