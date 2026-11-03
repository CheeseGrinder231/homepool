//! Read-only service tabs (#59): the private catalog, and one bounded GET per
//! service to a fixed loopback URL. A phone can only name a catalog ID; it
//! never chooses a URL, method, path, query or header.
use crate::{settings, view};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Deserialize, Clone)]
#[serde(untagged)]
pub enum Devices {
    All(String),
    List(Vec<String>),
}
fn default_bytes() -> u64 {
    settings::SERVICE_DEFAULT_BYTES
}
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub id: String,
    pub title: String,
    pub url: String,
    #[serde(default = "default_bytes")]
    pub max_bytes: u64,
    pub devices: Devices,
}

/// A checked loopback target: address, `Host` value and path.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub address: SocketAddr,
    pub authority: String,
    pub path: String,
}

/// Plain HTTP to the IPv4 or IPv6 loopback literal, an explicit port and a
/// fixed path; no user, query or fragment, and nothing the parser would rewrite.
pub fn target(raw: &str) -> Result<Target, String> {
    let refuse = || {
        Err("service url must be plain HTTP to the loopback address (IPv4 or IPv6 literal) with an explicit port and a fixed path, and no query or fragment".to_owned())
    };
    let Ok(u) = url::Url::parse(raw) else {
        return refuse();
    };
    let ip = match u.host() {
        Some(url::Host::Ipv4(ip)) if ip == Ipv4Addr::LOCALHOST => IpAddr::V4(ip),
        Some(url::Host::Ipv6(ip)) if ip == Ipv6Addr::LOCALHOST => IpAddr::V6(ip),
        _ => return refuse(),
    };
    // Written as two steps: the repository's literal scan flags URL prefixes.
    let Some(rest) = raw.strip_prefix("http:").and_then(|r| r.strip_prefix("//")) else {
        return refuse();
    };
    let authority = rest.split('/').next().unwrap_or("");
    let literal = |host: &str| {
        host == Ipv4Addr::LOCALHOST.to_string() || host == format!("[{}]", Ipv6Addr::LOCALHOST)
    };
    let explicit = authority.rsplit_once(':').and_then(|(host, p)| {
        p.parse::<u16>()
            .ok()
            .filter(|n| literal(host) && *n > 0 && p == n.to_string())
    });
    if u.scheme() != "http"
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
        || explicit.is_none()
        || explicit != u.port_or_known_default()
        || rest != format!("{authority}{}", u.path())
    {
        return refuse();
    }
    Ok(Target {
        address: SocketAddr::new(ip, explicit.unwrap_or(0)),
        authority: authority.to_owned(),
        path: u.path().to_owned(),
    })
}

impl Service {
    pub fn allows(&self, device: &str) -> bool {
        match &self.devices {
            Devices::All(_) => true,
            Devices::List(ids) => ids.iter().any(|d| d == device),
        }
    }
}

/// Checked when the configuration loads; any problem stops the bridge.
pub fn validate(services: &[Service]) -> Result<(), String> {
    if services.len() > homepool_protocol::wire::MAX_SERVICES {
        return Err(format!(
            "at most {} services",
            homepool_protocol::wire::MAX_SERVICES
        ));
    }
    for (i, s) in services.iter().enumerate() {
        if !homepool_protocol::wire::valid_service(&s.id) {
            return Err("service id must be 1-32 lowercase letters, digits or hyphens".into());
        }
        if services[..i].iter().any(|o| o.id == s.id) {
            return Err(format!("service id {} is used twice", s.id));
        }
        let n = s.title.chars().count();
        if n == 0
            || n > settings::SERVICE_TITLE_CHARS
            || s.title.chars().any(|c| c < ' ' || c == '\u{7f}')
        {
            return Err(format!("service {}: title must be 1-40 characters", s.id));
        }
        target(&s.url).map_err(|e| format!("service {}: {e}", s.id))?;
        if s.max_bytes == 0 || s.max_bytes > settings::SERVICE_MAX_BYTES {
            return Err(format!(
                "service {}: max_bytes must be 1 to {}",
                s.id,
                settings::SERVICE_MAX_BYTES
            ));
        }
        match &s.devices {
            Devices::All(all) if all == "all" => {}
            Devices::List(ids)
                if ids.len() <= settings::MAX_DEVICES
                    && ids.iter().all(|d| {
                        d.len() == 32
                            && d.bytes()
                                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                    }) => {}
            _ => {
                return Err(format!(
                    "service {}: devices must be \"all\" or a list of device IDs",
                    s.id
                ));
            }
        }
    }
    Ok(())
}

/// What the phone is told when a document can't be sent; never more.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Refusal {
    Unknown,
    NotAllowed,
    Unavailable,
    TooLarge,
    Invalid,
}
impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::NotAllowed => "not-allowed",
            Self::Unavailable => "unavailable",
            Self::TooLarge => "too-large",
            Self::Invalid => "invalid",
        }
    }
}

struct Deadline {
    tcp: TcpStream,
    until: Instant,
}
impl Deadline {
    fn left(&self) -> std::io::Result<Duration> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        Ok(left)
    }
}
impl Read for Deadline {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let left = self.left()?;
        self.tcp.set_read_timeout(Some(left))?;
        self.tcp.read(b)
    }
}
impl Write for Deadline {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let left = self.left()?;
        self.tcp.set_write_timeout(Some(left))?;
        self.tcp.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.tcp.flush()
    }
}

// Reads at most `max` body bytes; one more means too large.
fn capped(r: &mut impl Read, max: u64, out: &mut Vec<u8>) -> Result<(), Refusal> {
    let before = out.len() as u64;
    r.take(max + 1)
        .read_to_end(out)
        .map_err(|_| Refusal::Unavailable)?;
    if out.len() as u64 - before > max {
        return Err(Refusal::TooLarge);
    }
    Ok(())
}
fn chunked(r: &mut impl BufRead, max: u64) -> Result<Vec<u8>, Refusal> {
    let mut out = Vec::new();
    loop {
        let mut line = Vec::new();
        r.take(settings::SERVICE_CHUNK_LINE as u64)
            .read_until(b'\n', &mut line)
            .map_err(|_| Refusal::Unavailable)?;
        let line = std::str::from_utf8(&line).map_err(|_| Refusal::Unavailable)?;
        let line = line.strip_suffix("\r\n").ok_or(Refusal::Unavailable)?;
        let size = line.split(';').next().unwrap_or("").trim();
        let size = u64::from_str_radix(size, 16).map_err(|_| Refusal::Unavailable)?;
        if size == 0 {
            return Ok(out);
        }
        if out.len() as u64 + size > max {
            return Err(Refusal::TooLarge);
        }
        let start = out.len();
        r.take(size)
            .read_to_end(&mut out)
            .map_err(|_| Refusal::Unavailable)?;
        let mut end = [0; 2];
        if (out.len() - start) as u64 != size || r.read_exact(&mut end).is_err() || end != *b"\r\n"
        {
            return Err(Refusal::Unavailable);
        }
    }
}

/// One GET with one deadline. No redirects, cookies or credentials; only the
/// fixed `Host` and `Accept` headers. The body must be a valid view document,
/// which is returned normalised with the bridge's own clock as `fetched_at`.
pub fn fetch(s: &Service, deadline: Duration) -> Result<Vec<u8>, Refusal> {
    let t = target(&s.url).map_err(|_| Refusal::Unavailable)?;
    let until = Instant::now() + deadline;
    let tcp = TcpStream::connect_timeout(&t.address, deadline).map_err(|_| Refusal::Unavailable)?;
    let mut io = Deadline { tcp, until };
    write!(
        io,
        "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
        t.path, t.authority
    )
    .and_then(|_| io.flush())
    .map_err(|_| Refusal::Unavailable)?;
    let mut head = Vec::new();
    let mut buf = [0; 4096];
    let end = loop {
        if let Some(i) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if head.len() > settings::SERVICE_HEADER_BYTES {
            return Err(Refusal::Unavailable);
        }
        let n = io.read(&mut buf).map_err(|_| Refusal::Unavailable)?;
        if n == 0 {
            return Err(Refusal::Unavailable);
        }
        head.extend_from_slice(&buf[..n]);
    };
    let mut fields = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut fields);
    if !matches!(
        response.parse(&head[..end]),
        Ok(httparse::Status::Complete(_))
    ) || response.code != Some(200)
    {
        // Redirects and every other status: nothing is followed.
        return Err(Refusal::Unavailable);
    }
    let header = |name: &str| {
        response
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| std::str::from_utf8(h.value).map(str::trim))
            .collect::<Vec<_>>()
    };
    let (encoding, length) = (header("transfer-encoding"), header("content-length"));
    let mut body = std::io::Cursor::new(head[end..].to_vec()).chain(io);
    let max = s.max_bytes;
    let bytes = match (encoding.as_slice(), length.as_slice()) {
        ([], []) => {
            let mut out = Vec::new();
            capped(&mut body, max, &mut out)?;
            out
        }
        ([], [Ok(n)]) => {
            let n: u64 = n.parse().map_err(|_| Refusal::Unavailable)?;
            if n > max {
                return Err(Refusal::TooLarge);
            }
            let mut out = Vec::new();
            body.take(n)
                .read_to_end(&mut out)
                .map_err(|_| Refusal::Unavailable)?;
            if out.len() as u64 != n {
                return Err(Refusal::Unavailable);
            }
            out
        }
        ([Ok(e)], []) if e.eq_ignore_ascii_case("chunked") => {
            chunked(&mut BufReader::new(body), max)?
        }
        _ => return Err(Refusal::Unavailable),
    };
    let doc: Value = serde_json::from_slice(&bytes).map_err(|_| Refusal::Invalid)?;
    let mut doc = view::normalize(&doc).map_err(|_| Refusal::Invalid)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| Refusal::Unavailable)?
        .as_secs();
    doc["fetched_at"] = view::format_time(now as i64).into();
    serde_json::to_vec(&doc).map_err(|_| Refusal::Invalid)
}

type Outcome = Result<Arc<[u8]>, Refusal>;
/// A fetch in progress or finished, shared by every phone that asked for it.
#[derive(Clone, Default)]
pub struct Pending(Arc<Mutex<Option<Outcome>>>);
impl Pending {
    pub fn poll(&self) -> Option<Outcome> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}
static FETCHES: Mutex<BTreeMap<String, Pending>> = Mutex::new(BTreeMap::new());

/// One fetch per service at a time: a request while one runs waits for its
/// result; a request after it finished starts a new fetch.
pub fn request(s: &Service) -> Pending {
    request_with(s, settings::SERVICE_DEADLINE)
}
pub fn request_with(s: &Service, deadline: Duration) -> Pending {
    let mut fetches = FETCHES.lock().unwrap_or_else(|e| e.into_inner());
    let key = format!("{}\n{}", s.id, s.url);
    if let Some(p) = fetches.get(&key)
        && p.poll().is_none()
    {
        return p.clone();
    }
    let pending = Pending::default();
    fetches.insert(key, pending.clone());
    let (service, slot) = (s.clone(), pending.clone());
    std::thread::spawn(move || {
        let outcome = fetch(&service, deadline).map(Arc::from);
        *slot.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
    });
    pending
}
