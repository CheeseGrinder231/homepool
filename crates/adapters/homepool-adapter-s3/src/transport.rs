use crate::{Result, settings};
use std::{
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    sync::Arc,
    time::Instant,
};
use url::Url;
trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}
struct DeadlineTcp {
    tcp: TcpStream,
    deadline: Instant,
}
impl Read for DeadlineTcp {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.tcp.set_read_timeout(Some(left))?;
        self.tcp.read(b)
    }
}
impl Write for DeadlineTcp {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.tcp.set_write_timeout(Some(left))?;
        self.tcp.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.tcp.flush()
    }
}
pub struct Body {
    stream: Box<dyn Stream>,
    pub length: u64,
    remaining: u64,
    deadline: Instant,
}
impl Body {
    pub fn bytes(mut self, max: u64) -> Result<Vec<u8>> {
        if self.length > max {
            return Err("response bounds");
        }
        let mut b = Vec::new();
        self.read_to_end(&mut b).map_err(|_| "response")?;
        Ok(b)
    }
}
impl Read for Body {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        if Instant::now() > self.deadline {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        let size = b.len().min(self.remaining as usize);
        if size == 0 {
            return Ok(0);
        }
        let n = self.stream.read(&mut b[..size])?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}
static RESOLVING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct ResolvePermit;
impl Drop for ResolvePermit {
    fn drop(&mut self) {
        RESOLVING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}
fn resolve(host: &str, port: u16, deadline: Instant) -> Result<std::net::SocketAddr> {
    if Instant::now() >= deadline {
        return Err("resolve deadline");
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(std::net::SocketAddr::new(ip, port));
    }
    use std::sync::atomic::Ordering;
    RESOLVING
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < settings::RESOLVERS).then_some(n + 1)
        })
        .map_err(|_| "resolver capacity")?;
    let permit = ResolvePermit;
    let host = host.to_owned();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _permit = permit;
        let result = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|_| "resolve")
            .and_then(|mut addresses| addresses.next().ok_or("resolve"));
        let _ = sender.send(result);
    });
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| "resolve deadline")?
}
pub fn request(
    endpoint: &Url,
    method: &str,
    path: &str,
    query: &str,
    headers: &str,
    data: Option<(&mut std::fs::File, u64, &std::sync::atomic::AtomicBool)>,
) -> Result<Body> {
    let deadline = Instant::now() + settings::TIMEOUT;
    let host = endpoint.host_str().ok_or("endpoint")?;
    let port = endpoint.port_or_known_default().ok_or("endpoint")?;
    let address = resolve(host, port, deadline)?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("connect deadline");
    }
    let tcp = TcpStream::connect_timeout(&address, remaining).map_err(|_| "connect")?;
    tcp.set_read_timeout(Some(settings::TIMEOUT))
        .map_err(|_| "timeout")?;
    tcp.set_write_timeout(Some(settings::TIMEOUT))
        .map_err(|_| "timeout")?;
    let tcp = DeadlineTcp { tcp, deadline };
    let mut stream: Box<dyn Stream> = if endpoint.scheme() == "https" {
        let mut roots = rustls::RootCertStore::empty();
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        if roots.is_empty() {
            return Err("TLS roots");
        }
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = host.to_owned().try_into().map_err(|_| "TLS name")?;
        let connection =
            rustls::ClientConnection::new(Arc::new(config), name).map_err(|_| "TLS")?;
        Box::new(rustls::StreamOwned::new(connection, tcp))
    } else {
        Box::new(tcp)
    };
    if data
        .as_ref()
        .is_some_and(|(_, _, cancelled)| cancelled.load(std::sync::atomic::Ordering::SeqCst))
    {
        return Err("cancelled");
    }
    let length = data.as_ref().map_or(0, |(_, n, _)| *n);
    write!(stream,"{method} {path}{}{} HTTP/1.1\r\n{headers}content-length: {length}\r\nconnection: close\r\n\r\n",if query.is_empty(){""}else{"?"},query).map_err(|_|"request")?;
    if let Some((file, length, cancelled)) = data {
        let mut left = length;
        let mut buf = [0; 65536];
        while left > 0 {
            if cancelled.load(std::sync::atomic::Ordering::SeqCst) || Instant::now() > deadline {
                return Err("cancelled");
            }
            let size = buf.len().min(left as usize);
            file.read_exact(&mut buf[..size]).map_err(|_| "spool")?;
            stream.write_all(&buf[..size]).map_err(|_| "upload")?;
            left -= size as u64;
        }
    }
    stream.flush().map_err(|_| "request")?;
    let mut raw = Vec::new();
    let mut byte = [0];
    while !raw.ends_with(b"\r\n\r\n") {
        if raw.len() >= settings::HEADER_LIMIT || Instant::now() > deadline {
            return Err("headers");
        }
        stream.read_exact(&mut byte).map_err(|_| "headers")?;
        raw.push(byte[0]);
    }
    let mut fields = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut fields);
    response.parse(&raw).map_err(|_| "headers")?;
    if response.code == Some(404) {
        return Err("missing");
    }
    if !response.code.is_some_and(|c| (200..300).contains(&c)) {
        return Err("store refused");
    }
    if response
        .headers
        .iter()
        .any(|h| h.name.eq_ignore_ascii_case("transfer-encoding"))
    {
        return Err("transfer encoding");
    }
    let length = response
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("content-length"))
        .and_then(|h| std::str::from_utf8(h.value).ok()?.parse().ok())
        .ok_or("length")?;
    Ok(Body {
        stream,
        length,
        remaining: if method == "HEAD" { 0 } else { length },
        deadline,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expired_resolution_budget_fails_before_work() {
        assert_eq!(
            resolve("192.0.2.1", 80, Instant::now()).unwrap_err(),
            "resolve deadline"
        );
    }
    #[test]
    fn saturated_resolvers_refuse_without_allocating_another_worker() {
        use std::sync::atomic::Ordering;
        RESOLVING.store(settings::RESOLVERS, Ordering::SeqCst);
        let result = resolve("localhost", 80, Instant::now() + settings::TIMEOUT);
        let count = RESOLVING.swap(0, Ordering::SeqCst);
        assert_eq!(result.unwrap_err(), "resolver capacity");
        assert_eq!(count, settings::RESOLVERS);
    }
}
