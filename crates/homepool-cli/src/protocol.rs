//! One bounded JSON line per connection, with cooperative nonblocking I/O.
use crate::settings::{API_SIZE_LIMIT, HUB_POLL_INTERVAL, Timing};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

#[derive(Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Inventory {},
    Place {
        #[serde(flatten)]
        spec: Placement,
    },
    Logs {
        handle: String,
        #[serde(default)]
        tail: Option<usize>,
    },
    Release {
        handle: String,
    },
    Add {
        document: String,
    },
    Remove {
        member: u64,
    },
    Drain {
        member: u64,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    /// Only new identities, never retries, may bypass unknown members.
    #[serde(default)]
    pub fresh: bool,
    pub sandbox_id: String,
    pub image: String,
    pub cpu_millicores: u64,
    pub memory_bytes: u64,
    #[serde(default)]
    pub preferences: Vec<u64>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub environment: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize, Serialize)]
pub struct Response {
    pub version: u64,
    pub result: Option<Value>,
    pub error: Option<String>,
}
impl Response {
    pub fn from_result(result: Result<Value, String>) -> Self {
        match result {
            Ok(result) => Self {
                version: 1,
                result: Some(result),
                error: None,
            },
            Err(error) => Self {
                version: 1,
                result: None,
                error: Some(error),
            },
        }
    }
}

pub fn read_line(
    stream: &mut UnixStream,
    cancelled: &AtomicBool,
    deadline: Instant,
) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    loop {
        check(cancelled, deadline)?;
        let mut buffer = [0; 8192];
        match stream.read(&mut buffer) {
            Ok(0) => return Err("API connection ended before newline".into()),
            Ok(n) => {
                if output.len() + n > API_SIZE_LIMIT {
                    return Err("API message exceeds size limit".into());
                }
                output.extend_from_slice(&buffer[..n]);
                if let Some(end) = output.iter().position(|&b| b == b'\n') {
                    if end + 1 != output.len() {
                        return Err("send one JSON line per connection".into());
                    }
                    return Ok(output);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(HUB_POLL_INTERVAL)
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err("could not read API connection".into()),
        }
    }
}

pub fn write_line(
    stream: &mut UnixStream,
    bytes: &[u8],
    cancelled: &AtomicBool,
    deadline: Instant,
) -> Result<(), String> {
    let mut offset = 0;
    while offset < bytes.len() {
        check(cancelled, deadline)?;
        match stream.write(&bytes[offset..]) {
            Ok(0) => return Err("API connection closed".into()),
            Ok(n) => offset += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(HUB_POLL_INTERVAL)
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err("could not write API connection".into()),
        }
    }
    Ok(())
}
pub fn check(cancelled: &AtomicBool, deadline: Instant) -> Result<(), String> {
    if cancelled.load(Ordering::Acquire) {
        Err("hub cancelled".into())
    } else if Instant::now() >= deadline {
        Err("API deadline exceeded".into())
    } else {
        Ok(())
    }
}

pub fn bytes(value: &impl Serialize) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec(value).map_err(|_| "could not encode API message")?;
    bytes.push(b'\n');
    if bytes.len() > API_SIZE_LIMIT {
        return Err("API message exceeds size limit".into());
    }
    Ok(bytes)
}

/// One absolute local monotonic deadline includes time spent in the socket
/// queue, and is unaffected by changes to the wall clock.
#[derive(Deserialize, Serialize)]
pub struct Message {
    pub deadline_monotonic_ms: u64,
    #[serde(flatten)]
    pub request: Request,
}
fn monotonic_ms() -> Result<u64, String> {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    u64::try_from(time.tv_sec)
        .ok()
        .and_then(|s| s.checked_mul(1000))
        .and_then(|ms| ms.checked_add(time.tv_nsec as u64 / 1_000_000))
        .ok_or("invalid monotonic clock".into())
}
impl Message {
    pub fn new(request: Request, timeout: std::time::Duration) -> Result<Self, String> {
        let duration =
            u64::try_from(timeout.as_millis()).map_err(|_| "invalid request deadline")?;
        let deadline_monotonic_ms = monotonic_ms()?
            .checked_add(duration)
            .ok_or("invalid request deadline")?;
        Ok(Self {
            deadline_monotonic_ms,
            request,
        })
    }
    pub fn deadline(&self, ceiling: Instant) -> Result<Instant, String> {
        let now = Instant::now();
        let remaining = self
            .deadline_monotonic_ms
            .checked_sub(monotonic_ms()?)
            .ok_or("API deadline exceeded")?;
        Ok(now
            .checked_add(std::time::Duration::from_millis(remaining))
            .ok_or("invalid request deadline")?
            .min(ceiling))
    }
}

pub struct Budget {
    pub deadline: Instant,
    pub timing: Timing,
    pub cancelled: Arc<AtomicBool>,
}
impl Budget {
    pub fn check(&self) -> Result<(), String> {
        check(&self.cancelled, self.deadline)
    }
}

pub fn connect(
    socket: &std::path::Path,
    cancelled: &AtomicBool,
    deadline: Instant,
) -> Result<UnixStream, String> {
    use rustix::net::{
        AddressFamily, SocketAddrUnix, SocketFlags, SocketType, connect as connect_socket,
        socket_with,
    };
    let address = SocketAddrUnix::new(socket).map_err(|_| "invalid hub socket address")?;
    loop {
        check(cancelled, deadline)?;
        let fd = socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
            None,
        )
        .map_err(|_| "could not create API stream")?;
        match connect_socket(&fd, &address) {
            Ok(()) => return Ok(UnixStream::from(fd)),
            Err(
                rustix::io::Errno::AGAIN | rustix::io::Errno::INPROGRESS | rustix::io::Errno::INTR,
            ) => std::thread::sleep(HUB_POLL_INTERVAL),
            Err(_) => return Err("could not connect to hub socket".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn placement_socket_schema_accepts_only_the_profile_name() {
        let base = serde_json::json!({"deadline_monotonic_ms":1,"op":"place","sandbox_id":"example","image":"alpine:3.22","cpu_millicores":100,"memory_bytes":33554432,"profile":"runner"});
        assert!(serde_json::from_value::<super::Message>(base.clone()).is_ok());
        for (key, value) in [
            ("privileged", serde_json::json!(true)),
            ("binds", serde_json::json!([])),
            ("volumes", serde_json::json!([])),
            ("flags", serde_json::json!([])),
            ("profile", serde_json::json!({"privileged":true})),
        ] {
            let mut input = base.clone();
            input[key] = value;
            assert!(serde_json::from_value::<super::Message>(input).is_err());
        }
    }

    #[test]
    fn logs_schema_and_worst_case_response_fit_existing_framing() {
        use super::{Message, Request, Response, bytes};
        use crate::settings::{API_SIZE_LIMIT, LOG_BYTES};
        use serde_json::json;
        for tail in [json!(-1), json!("all")] {
            assert!(
                serde_json::from_value::<Request>(
                    json!({"op":"logs", "handle":"1:example", "tail":tail})
                )
                .is_err()
            );
        }
        assert!(
            serde_json::from_str::<Request>(
                r#"{"op":"logs","handle":"1:example","tail":18446744073709551616}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<Request>(
                json!({"op":"logs", "handle":"1:example", "follow":true})
            )
            .is_err()
        );
        let request = Message::new(
            Request::Logs {
                handle: format!("{}:{}", u64::MAX, "a".repeat(64)),
                tail: Some(1000),
            },
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        assert!(bytes(&request).unwrap().len() < API_SIZE_LIMIT);
        let response = Response::from_result(Ok(json!({
            "stdout": "\0".repeat(LOG_BYTES / 2),
            "stderr": "\u{0001}".repeat(LOG_BYTES / 2), "cut": true
        })));
        let wire = bytes(&response).unwrap();
        assert!(wire.len() <= API_SIZE_LIMIT);
        assert_eq!(
            serde_json::from_slice::<Response>(&wire).unwrap().version,
            1
        );
        let invalid = Response::from_result(Ok(
            json!({"stdout": "\u{fffd}".repeat(LOG_BYTES), "stderr":"", "cut":true}),
        ));
        assert!(bytes(&invalid).unwrap().len() <= API_SIZE_LIMIT);
        let oversized = Request::Logs {
            handle: "a".repeat(API_SIZE_LIMIT),
            tail: None,
        };
        assert!(bytes(&oversized).is_err());
    }

    #[test]
    fn inventory_rejects_unknown_fields() {
        assert!(
            serde_json::from_str::<super::Request>(r#"{"op":"inventory","preferences":[999]}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<super::Request>(r#"{"op":"inventory"}"#).is_ok());
        assert!(
            serde_json::from_str::<super::Message>(
                r#"{"deadline_monotonic_ms":1,"op":"inventory","unexpected":true}"#
            )
            .is_err()
        );
    }
}
