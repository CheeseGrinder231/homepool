//! Loopback HTTP facade. Authentication and routing precede every hub call.
use crate::{
    hub,
    jobs::{Jobs, encode_inventory},
    private,
    protocol::Request,
    settings::*,
};
use std::{
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
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
            let bytes = private::read_bounded(path, HTTP_TOKEN_BYTES + 2).ok()?;
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

pub fn serve(
    listen: SocketAddr,
    token_file: &Path,
    operations_file: Option<&Path>,
    socket: &Path,
) -> Result<(), String> {
    if !listen.ip().is_loopback() {
        return Err("API listen address must be loopback".into());
    }
    let token = Arc::new(Token::load(token_file)?);
    let jobs = Arc::new(Jobs::load(operations_file)?);
    // One submitting API per hub socket, across process lifetimes. This lock
    // contains no job data; Docker remains the only durable job record.
    let _writer = if operations_file.is_some() {
        private::parent(socket)?;
        Some(
            private::lock(socket)
                .map_err(|_| "another API owns this hub socket's submission lock")?,
        )
    } else {
        None
    };
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
        let jobs = Arc::clone(&jobs);
        let socket = socket.to_owned();
        let accepted = Instant::now();
        std::thread::Builder::new()
            .spawn(move || {
                let _slot = slot;
                handle(stream, &token, &jobs, &socket, accepted, timing);
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

fn handle(
    mut stream: TcpStream,
    token: &Token,
    jobs: &Jobs,
    socket: &Path,
    accepted: Instant,
    timing: Timing,
) {
    let input_end = accepted + HTTP_IO_TIMEOUT;
    let mut post_resource = false;
    let result = read_request(&mut stream, accepted + HTTP_HEADER_TIMEOUT).and_then(|bytes| {
        post_resource = bytes.split(|b| *b == b' ').nth(1) == Some(b"/v1/jobs");
        route(
            &bytes,
            &mut stream,
            token,
            jobs,
            socket,
            input_end,
            accepted + timing.request,
        )
    });
    let (status, body) = result.unwrap_or_else(|status| (status, Vec::new()));
    // Hub calls have their existing total request budget. Slow reads and writes
    // cannot extend this absolute connection ceiling by making tiny progress.
    let Some(end) = input_end
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
        409 => "Conflict",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Service Unavailable",
    };
    let extra = match status {
        401 => "WWW-Authenticate: Bearer\r\n",
        405 if post_resource => "Allow: POST\r\n",
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
    // Deliver the response's FIN before closing with unread request bytes.
    // Otherwise early rejections (especially 413) can reset away the response.
    // Only consume bytes already available: a peer that keeps its write side
    // open must not retain the connection slot after receiving the response.
    let _ = stream.shutdown(Shutdown::Write);
    if stream.set_nonblocking(true).is_err() {
        return;
    }
    let mut drained = 0;
    while drained < HTTP_DRAIN_BYTES {
        let mut buffer = [0; 1024];
        let remaining = buffer.len().min(HTTP_DRAIN_BYTES - drained);
        match stream.read(&mut buffer[..remaining]) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained += n,
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
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            return if end + 4 <= HTTP_HEADER_BYTES {
                Ok(bytes)
            } else {
                Err(431)
            };
        }
        if bytes.len() > HTTP_HEADER_BYTES {
            return Err(431);
        }
    }
}

fn route(
    bytes: &[u8],
    stream: &mut TcpStream,
    token: &Token,
    jobs: &Jobs,
    socket: &Path,
    input_end: Instant,
    deadline: Instant,
) -> Result<(u16, Vec<u8>), u16> {
    let mut headers = [httparse::EMPTY_HEADER; HTTP_HEADER_COUNT];
    let mut request = httparse::Request::new(&mut headers);
    let header_bytes = match request.parse(bytes) {
        Ok(httparse::Status::Complete(n)) => n,
        Err(httparse::Error::TooManyHeaders) => return Err(431),
        _ => return Err(400),
    };
    let path = request.path.ok_or(400u16)?;
    let job_id = path.strip_prefix("/v1/jobs/");
    if !["/v1/health", "/v1/sandboxes", "/v1/operations", "/v1/jobs"].contains(&path)
        && job_id.is_none()
    {
        return Err(404);
    }
    if request.method != Some(if path == "/v1/jobs" { "POST" } else { "GET" }) {
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
    let mut lengths = request
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("content-length"));
    let length = lengths
        .next()
        .map(|h| {
            if h.value.is_empty() || !h.value.iter().all(u8::is_ascii_digit) {
                return Err(400u16);
            }
            std::str::from_utf8(h.value)
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .ok_or(413u16)
        })
        .transpose()?;
    if lengths.next().is_some()
        || request
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("transfer-encoding"))
    {
        return Err(400);
    }
    if path == "/v1/jobs" {
        let length = length.ok_or(400u16)?;
        if length > HTTP_BODY_BYTES {
            return Err(413);
        }
        let mut body = bytes[header_bytes..].to_vec();
        if body.len() > length {
            return Err(400);
        }
        while body.len() < length {
            let wait = input_end.saturating_duration_since(Instant::now());
            if wait.is_zero() {
                return Err(408);
            }
            stream.set_read_timeout(Some(wait)).map_err(|_| 400u16)?;
            let mut buffer = [0; 1024];
            let remaining = buffer.len().min(length - body.len());
            let n = stream.read(&mut buffer[..remaining]).map_err(|_| 408u16)?;
            if n == 0 {
                return Err(400);
            }
            body.extend_from_slice(&buffer[..n]);
        }
        return jobs.submit(socket, &body, deadline);
    }
    if length.is_some_and(|n| n != 0) || bytes.len() != header_bytes {
        return Err(400);
    }
    if path == "/v1/operations" {
        return jobs.names().map(|b| (200, b));
    }
    if let Some(id) = job_id {
        return jobs.get(socket, id, deadline);
    }
    let value = hub::call_until(socket, Request::Inventory {}, deadline).map_err(|_| 503u16)?;
    encode_inventory(value).map(|body| (200, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unauthorized_responses_release_slots_with_peers_still_open() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let token = Token::parse(b"example-token").unwrap();
        let jobs = Jobs::load(None).unwrap();
        let mut peers = Vec::new();
        for _ in 0..=HTTP_CONNECTIONS {
            let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            peer.set_read_timeout(Some(std::time::Duration::from_secs(30)))
                .unwrap();
            peer.write_all(b"POST /v1/jobs HTTP/1.1\r\nContent-Length: 4096\r\n\r\n")
                .unwrap();
            let (stream, _) = listener.accept().unwrap();
            let inspection = stream.try_clone().unwrap();
            active.fetch_add(1, Ordering::AcqRel);
            {
                let _slot = ConnectionSlot(Arc::clone(&active));
                handle(
                    stream,
                    &token,
                    &jobs,
                    Path::new("unused.sock"),
                    Instant::now(),
                    Timing {
                        operation: API_TIMEOUT,
                        request: API_TIMEOUT,
                    },
                );
            }
            // Inspect the OS flag instead of racing a five-second drain timer.
            // No body or peer EOF has arrived, so a blocking drain would wait.
            assert!(
                rustix::fs::fcntl_getfl(&inspection)
                    .unwrap()
                    .contains(rustix::fs::OFlags::NONBLOCK)
            );
            assert_eq!(active.load(Ordering::Acquire), 0);
            drop(inspection);
            let mut response = String::new();
            peer.read_to_string(&mut response).unwrap();
            assert!(response.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
            assert!(response.contains("WWW-Authenticate: Bearer\r\n"));
            peers.push(peer);
        }
    }

    #[test]
    fn early_errors_are_delivered_with_more_unread_input_than_the_drain_cap() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let token = Token::parse(b"example-token").unwrap();
        let jobs = Jobs::load(None).unwrap();
        for (authorization, status) in [("", 401), ("Authorization: Bearer example-token\r\n", 413)]
        {
            let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            peer.set_read_timeout(Some(std::time::Duration::from_secs(30)))
                .unwrap();
            peer.set_write_timeout(Some(std::time::Duration::from_secs(30)))
                .unwrap();
            peer.write_all(
                format!("POST /v1/jobs HTTP/1.1\r\n{authorization}Content-Length: 24576\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
            peer.write_all(&vec![b'x'; HTTP_DRAIN_BYTES * 2]).unwrap();
            let (stream, _) = listener.accept().unwrap();
            handle(
                stream,
                &token,
                &jobs,
                Path::new("unused.sock"),
                Instant::now(),
                Timing {
                    operation: API_TIMEOUT,
                    request: API_TIMEOUT,
                },
            );
            let mut response = String::new();
            if let Err(error) = peer.read_to_string(&mut response) {
                assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
            }
            assert!(response.starts_with(&format!("HTTP/1.1 {status} ")));
            assert!(response.ends_with("\r\n\r\n"));
            assert!(response.contains("Content-Length: 0\r\n"));
        }
    }

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
