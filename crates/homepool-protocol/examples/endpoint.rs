//! Line-oriented, offline test endpoint; never used by the bridge.
use homepool_protocol::{Mode, Session, wire};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{self, BufRead, Write},
};
fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("hex".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| "hex".into()))
        .collect()
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn run(
    v: Value,
    s: &mut HashMap<u64, Session>,
    transfers: &mut HashMap<(u64, String), wire::Transfer>,
) -> Result<Value, String> {
    let id = v["session"].as_u64().unwrap_or(1);
    match v["op"].as_str().ok_or("op")? {
        "new" => {
            let mode = match v["mode"].as_str() {
                Some("enroll") => Mode::Enroll,
                Some("reconnect") => Mode::Reconnect,
                _ => return Err("mode".into()),
            };
            let private = unhex(v["private"].as_str().ok_or("private")?)?;
            let remote = v["remote"].as_str().map(unhex).transpose()?;
            let psk = v["psk"].as_str().map(unhex).transpose()?;
            s.insert(
                id,
                Session::new(
                    mode,
                    v["initiator"] == true,
                    v["route"].as_str().ok_or("route")?,
                    &private,
                    remote.as_deref(),
                    psk.as_deref(),
                )?,
            );
            Ok(json!(true))
        }
        "write" | "read" | "encrypt" | "decrypt" => {
            let op = v["op"].as_str().unwrap();
            let b = unhex(v["data"].as_str().unwrap_or(""))?;
            let out = s.get_mut(&id).ok_or("session")?.message(
                op == "read" || op == "decrypt",
                op == "write" || op == "read",
                &b,
            )?;
            Ok(json!(hex(&out)))
        }
        "receive" => {
            let input = unhex(v["data"].as_str().ok_or("data")?)?;
            let plain = s
                .get_mut(&id)
                .ok_or("session")?
                .message(true, false, &input)?;
            let m = wire::decode(&plain)?;
            let key = (id, m["id"].as_str().ok_or("id")?.to_owned());
            match m["type"].as_str() {
                Some("put-begin") => {
                    if transfers.contains_key(&key) {
                        return Err("duplicate transfer".into());
                    }
                    transfers.insert(
                        key,
                        wire::Transfer::new(
                            m["id"].as_str().unwrap().into(),
                            m["length"].as_u64().ok_or("length")?,
                        )?,
                    );
                }
                Some("chunk" | "put-end") => {
                    transfers.get_mut(&key).ok_or("transfer")?.accept(&m)?;
                }
                _ => {}
            }
            Ok(json!(hex(&plain)))
        }
        "encode" => Ok(json!(hex(&wire::encode(&v["message"])?))),
        "decode" => wire::decode(&unhex(v["data"].as_str().ok_or("data")?)?),
        "envelope" => {
            let b = unhex(v["data"].as_str().ok_or("data")?)?;
            let (k, s, d) = wire::unwrap(&b)?;
            Ok(json!(hex(&wire::envelope(k, s, d)?)))
        }
        "admission" => {
            let b = unhex(v["data"].as_str().ok_or("data")?)?;
            let (r, t) = wire::read_admission(&b)?;
            Ok(json!(hex(&wire::admission(r, t)?)))
        }
        _ => Err("op".into()),
    }
}
fn main() {
    let mut sessions = HashMap::new();
    let mut transfers = HashMap::new();
    let mut out = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let result = line
            .map_err(|_| "input".into())
            .and_then(|s| serde_json::from_str(&s).map_err(|_| "json".into()))
            .and_then(|v| run(v, &mut sessions, &mut transfers));
        let response = match result {
            Ok(v) => json!({"ok":v}),
            Err(e) => json!({"error":e}),
        };
        writeln!(out, "{response}").unwrap();
        out.flush().unwrap();
    }
}
