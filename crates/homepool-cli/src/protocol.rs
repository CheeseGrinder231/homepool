//! One bounded JSON line per connection, with cooperative nonblocking I/O.
use crate::settings::{API_SIZE_LIMIT, API_TIMEOUT, HUB_POLL_INTERVAL};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

#[derive(Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Inventory,
    Place {
        #[serde(flatten)]
        spec: Placement,
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
    pub sandbox_id: String,
    pub image: String,
    pub cpu_millicores: u64,
    pub memory_bytes: u64,
    #[serde(default)]
    pub preferences: Vec<u64>,
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

pub fn read_line(stream: &mut UnixStream, cancelled: &AtomicBool) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + API_TIMEOUT;
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
) -> Result<(), String> {
    let deadline = Instant::now() + API_TIMEOUT;
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
fn check(cancelled: &AtomicBool, deadline: Instant) -> Result<(), String> {
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
