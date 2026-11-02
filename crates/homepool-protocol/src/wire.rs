//! Closed positional binary schema. See docs/off-lan-protocol.md.
use serde_json::{Value, json};
const SCHEMA: &str = include_str!("../../../protocol/schema.json");
fn schema() -> Vec<(String, Vec<(String, String)>)> {
    serde_json::from_str(SCHEMA).expect("static schema")
}
fn bound(k: &str) -> Option<u64> {
    match k {
        "length" => Some(9_007_199_254_740_991),
        "count" => Some(u32::MAX.into()),
        "page" => Some(100),
        "credit" => Some(16),
        _ => None,
    }
}
fn limit(k: &str) -> usize {
    match k {
        "id" | "code" => 32,
        "hash" | "token" => 64,
        "key" => 512,
        "name" => 255,
        "mime" => 127,
        "cursor" => 1024,
        _ => 0,
    }
}
fn valid(k: &str, v: &Value) -> Result<(), String> {
    if let Some(max) = bound(k) {
        let n = v.as_u64().ok_or("integer")?;
        if n > max || ((k == "page" || k == "credit") && n == 0) {
            return Err("integer".into());
        }
        return Ok(());
    }
    if k == "data" {
        let a = v.as_array().ok_or("data")?;
        if a.is_empty() || a.len() > 61440 || a.iter().any(|x| x.as_u64().is_none_or(|n| n > 255)) {
            return Err("chunk".into());
        }
        return Ok(());
    }
    if k == "items" {
        let a = v.as_array().ok_or("items")?;
        if a.len() > 100 {
            return Err("items".into());
        }
        for item in a {
            let o = item.as_object().ok_or("item")?;
            if o.len() != 3 {
                return Err("item fields".into());
            }
            for (f, t) in [("key", "key"), ("length", "length"), ("type", "mime")] {
                valid(t, o.get(f).ok_or("item field")?)?;
            }
        }
        return Ok(());
    }
    let s = v.as_str().ok_or("string")?;
    if s.len() > limit(k) || s.chars().any(|c| c < ' ' || c == '\u{7f}') {
        return Err("string".into());
    }
    if matches!(k, "id" | "hash" | "token")
        && (s.len() != limit(k)
            || !s
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
    {
        return Err("hex".into());
    }
    if k == "name" && s.is_empty() {
        return Err("name".into());
    }
    if k == "mime" && !s.is_ascii() {
        return Err("mime".into());
    }
    if k == "code" && (s.is_empty() || !s.bytes().all(|c| c.is_ascii_lowercase() || c == b'-')) {
        return Err("code".into());
    }
    if k == "key" && !valid_key(s) {
        return Err("key".into());
    }
    Ok(())
}
fn valid_key(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 46 || b.len() > 118 {
        return false;
    }
    for (i, c) in b.iter().enumerate() {
        let ok = match i {
            4 | 7 => *c == b'-',
            10 => *c == b'/',
            17 | 44 => *c == b'-',
            0..=16 => c.is_ascii_digit(),
            18..=42 => c.is_ascii_lowercase() || (b'2'..=b'7').contains(c),
            43 => b"aeimquy4".contains(c),
            _ => c.is_ascii_alphanumeric() || b"._-".contains(c),
        };
        if !ok {
            return false;
        }
    }
    !b".-".contains(&b[45]) && !b".-".contains(b.last().unwrap())
}

fn put(k: &str, v: &Value, out: &mut Vec<u8>) -> Result<(), String> {
    valid(k, v)?;
    if bound(k).is_some() {
        let n = v.as_u64().ok_or("integer")?.to_be_bytes();
        out.extend_from_slice(&n[if k == "length" { 0 } else { 4 }..]);
    } else if k == "items" {
        let a = v.as_array().ok_or("items")?;
        out.extend_from_slice(&(a.len() as u16).to_be_bytes());
        for i in a {
            put("key", &i["key"], out)?;
            put("length", &i["length"], out)?;
            put("mime", &i["type"], out)?;
        }
    } else {
        let b = if k == "data" {
            v.as_array()
                .ok_or("data")?
                .iter()
                .map(|x| x.as_u64().unwrap() as u8)
                .collect()
        } else {
            v.as_str().ok_or("string")?.as_bytes().to_vec()
        };
        out.extend_from_slice(&(b.len() as u16).to_be_bytes());
        out.extend(b);
    }
    Ok(())
}
pub fn encode(v: &Value) -> Result<Vec<u8>, String> {
    let schemas = schema();
    let (idx, (_, fields)) = schemas
        .iter()
        .enumerate()
        .find(|(_, (name, _))| v["type"] == *name)
        .ok_or("unknown type")?;
    let o = v.as_object().ok_or("object")?;
    if o.len() != fields.len() + 1 {
        return Err("fields".into());
    }
    let mut out = vec![1, (idx + 1) as u8];
    for (f, k) in fields {
        put(k, o.get(f).ok_or("field")?, &mut out)?;
    }
    if out.len() > 65519 {
        return Err("size".into());
    }
    Ok(out)
}
fn take<'a>(b: &mut &'a [u8], n: usize) -> Result<&'a [u8], String> {
    if b.len() < n {
        return Err("truncated".into());
    }
    let (a, rest) = b.split_at(n);
    *b = rest;
    Ok(a)
}
fn num(b: &mut &[u8], n: usize) -> Result<u64, String> {
    let mut x = [0; 8];
    x[8 - n..].copy_from_slice(take(b, n)?);
    Ok(u64::from_be_bytes(x))
}
fn read(k: &str, b: &mut &[u8]) -> Result<Value, String> {
    let v = if bound(k).is_some() {
        json!(num(b, if k == "length" { 8 } else { 4 })?)
    } else if k == "items" {
        let n = num(b, 2)?;
        if n > 100 {
            return Err("items".into());
        }
        let mut a = Vec::new();
        for _ in 0..n {
            a.push(json!({"key":read("key",b)?,"length":read("length",b)?,"type":read("mime",b)?}));
        }
        json!(a)
    } else {
        let n = num(b, 2)? as usize;
        let x = take(b, n)?;
        if k == "data" {
            json!(x)
        } else {
            json!(std::str::from_utf8(x).map_err(|_| "utf8")?)
        }
    };
    valid(k, &v)?;
    Ok(v)
}
pub fn decode(mut b: &[u8]) -> Result<Value, String> {
    if b.len() < 2 || b.len() > 65519 || b[0] != 1 {
        return Err("header".into());
    }
    let schemas = schema();
    let (name, fields) = schemas.get(b[1].wrapping_sub(1) as usize).ok_or("type")?;
    b = &b[2..];
    let mut out = json!({"type":name});
    for (f, k) in fields {
        out[f] = read(k, &mut b)?;
    }
    if !b.is_empty() {
        return Err("trailing".into());
    }
    Ok(out)
}
pub fn envelope(kind: u8, session: u32, data: &[u8]) -> Result<Vec<u8>, String> {
    let size = match kind {
        0 => None,
        1 | 5 => Some(4),
        2 | 3 => Some(0),
        4 => Some(45),
        6 | 7 => Some(8),
        _ => return Err("envelope".into()),
    };
    if (if kind >= 4 {
        session != 0
    } else {
        session == 0
    }) || data.len() > 65535
        || size.is_some_and(|n| data.len() != n)
        || (kind == 4 && data[32] > 1)
    {
        return Err("envelope".into());
    }
    let mut b = vec![1, kind];
    b.extend(session.to_be_bytes());
    b.extend(data);
    Ok(b)
}
pub fn unwrap(b: &[u8]) -> Result<(u8, u32, &[u8]), String> {
    if b.len() < 6 || b.len() > 65541 || b[0] != 1 {
        return Err("envelope".into());
    }
    let session = u32::from_be_bytes(b[2..6].try_into().unwrap());
    envelope(b[1], session, &b[6..])?;
    Ok((b[1], session, &b[6..]))
}
pub fn admission(role: u8, token: &[u8]) -> Result<Vec<u8>, String> {
    if role > 1 || token.len() != 32 {
        return Err("admission".into());
    }
    let mut b = vec![1, role];
    b.extend(token);
    Ok(b)
}
pub fn read_admission(b: &[u8]) -> Result<(u8, &[u8]), String> {
    if b.len() != 34 || b[0] != 1 {
        return Err("admission".into());
    }
    admission(b[1], &b[2..])?;
    Ok((b[1], &b[2..]))
}
pub struct Transfer {
    id: String,
    length: u64,
    received: u64,
    count: u64,
    closed: bool,
}
impl Transfer {
    pub fn new(id: String, length: u64) -> Result<Self, String> {
        valid("id", &json!(id))?;
        valid("length", &json!(length))?;
        Ok(Self {
            id,
            length,
            received: 0,
            count: 0,
            closed: false,
        })
    }
    pub fn accept(&mut self, m: &Value) -> Result<bool, String> {
        let result = self.next(m);
        if result.is_err() {
            self.closed = true;
        }
        result
    }
    fn next(&mut self, m: &Value) -> Result<bool, String> {
        encode(m)?;
        if self.closed || m["id"] != self.id {
            return Err("transfer".into());
        }
        if m["type"] == "chunk" {
            let n = m["data"].as_array().ok_or("data")?.len() as u64;
            if m["index"] != self.count || self.received + n > self.length {
                return Err("chunk order/size".into());
            }
            self.count += 1;
            self.received += n;
            return Ok(false);
        }
        if (m["type"] != "put-end" && m["type"] != "get-end")
            || m["length"] != self.length
            || self.received != self.length
            || m["count"] != self.count
        {
            return Err("end mismatch".into());
        }
        self.closed = true;
        Ok(true)
    }
}
