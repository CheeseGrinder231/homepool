use crate::{
    peer::Peer,
    settings,
    state::{self, Config, Identity, Result, State},
};
use homepool_protocol::wire;
use serde_json::{Value, json};
use std::net::ToSocketAddrs;
use std::os::unix::fs::OpenOptionsExt;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpStream,
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    time::Instant,
};
use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};
type Socket = WebSocket<MaybeTlsStream<TcpStream>>;
fn send(ws: &mut Socket, kind: u8, id: u32, data: &[u8]) -> Result<()> {
    ws.send(Message::Binary(wire::envelope(kind, id, data)?.into()))
        .map_err(|_| "relay send".into())
}
fn register(ws: &mut Socket, s: &State) -> Result<()> {
    for binding in s.retired.iter().copied().chain(
        s.invitations
            .values()
            .filter(|i| i.expiry <= state::now())
            .map(|i| i.binding),
    ) {
        send(ws, 5, 0, &binding.to_be_bytes())?;
        std::thread::sleep(settings::TOKEN_INTERVAL);
    }
    for i in s.invitations.values().filter(|i| i.expiry > state::now()) {
        token(ws, &i.token, 0, i.binding, i.expiry)?;
    }
    for d in s.devices.values() {
        token(ws, &d.token, 1, d.binding, 0)?;
    }
    Ok(())
}
fn token(ws: &mut Socket, token: &str, class: u8, binding: u32, expiry: u64) -> Result<()> {
    let mut b = state::hash(token)?;
    b.push(class);
    b.extend(binding.to_be_bytes());
    b.extend(expiry.to_be_bytes());
    std::thread::sleep(settings::TOKEN_INTERVAL);
    send(ws, 4, 0, &b)
}
fn connect(c: &Config) -> Result<Socket> {
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(65541))
        .max_frame_size(Some(65541))
        .max_write_buffer_size(1024 * 1024);
    let url = url::Url::parse(&c.relay_url).map_err(|_| "relay URL")?;
    let address = (
        url.host_str().ok_or("relay host")?,
        url.port_or_known_default().ok_or("relay port")?,
    )
        .to_socket_addrs()
        .map_err(|_| "relay resolve")?
        .next()
        .ok_or("relay resolve")?;
    let tcp = TcpStream::connect_timeout(&address, settings::PONG).map_err(|_| "relay connect")?;
    tcp.set_read_timeout(Some(settings::PONG))
        .map_err(|_| "relay timeout")?;
    tcp.set_write_timeout(Some(settings::PONG))
        .map_err(|_| "relay timeout")?;
    let (mut ws, _) =
        tungstenite::client_tls_with_config(c.relay_url.as_str(), tcp, Some(config), None)
            .map_err(|_| "relay handshake")?;
    let tcp = match ws.get_mut() {
        MaybeTlsStream::Plain(tcp) => tcp,
        MaybeTlsStream::Rustls(tls) => &mut tls.sock,
        _ => return Err("TLS stream".into()),
    };
    tcp.set_read_timeout(Some(settings::TICK))
        .map_err(|_| "relay timeout")?;
    tcp.set_write_timeout(Some(settings::PONG))
        .map_err(|_| "relay timeout")?;
    ws.send(Message::Binary(wire::admission(1, &c.token()?)?.into()))
        .map_err(|_| "admission")?;
    Ok(ws)
}
pub fn run(c: &Config) -> Result<()> {
    let identity = Identity::load(&c.state_dir)?;
    let mut s = State::load(&c.state_dir)?;

    let lock = std::fs::OpenOptions::new()
        .create(true)
        .mode(0o600)
        .truncate(false)
        .write(true)
        .open(c.state_dir.join("lock"))
        .map_err(|_| "lock")?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| "already running")?;
    crate::operations::reconcile(c, &mut s)?;
    let path = c.state_dir.join("control.sock");
    if path.exists() {
        std::fs::remove_file(&path).map_err(|_| "control cleanup")?;
    }
    std::env::set_current_dir(&c.state_dir).map_err(|_| "state directory")?;
    let listener = UnixListener::bind("control.sock").map_err(|_| "control")?;
    listener.set_nonblocking(true).map_err(|_| "control")?;
    let mut backoff = 1u64;
    loop {
        let config = c.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = sender.send(connect(&config));
        });
        let result = loop {
            match receiver.try_recv() {
                Ok(result) => break result,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    control(c, &identity, &mut s, &listener, None, &mut BTreeMap::new())?;
                    std::thread::sleep(settings::TICK);
                }
                Err(_) => break Err("relay connector".into()),
            }
        };
        if let Ok(mut ws) = result
            && register(&mut ws, &s).is_ok()
        {
            backoff = 1;
            let _ = connected(c, &identity, &mut s, &listener, &mut ws);
        }
        let jitter = u64::from(state::unhex(&state::random(1)?)?[0]);
        let until = Instant::now() + std::time::Duration::from_millis(backoff * 1000 + jitter);
        while Instant::now() < until {
            control(c, &identity, &mut s, &listener, None, &mut BTreeMap::new())?;
            std::thread::sleep(settings::TICK);
        }
        backoff = (backoff * 2).min(60);
    }
}
fn connected(
    c: &Config,
    identity: &Identity,
    s: &mut State,
    listener: &UnixListener,
    ws: &mut Socket,
) -> Result<()> {
    let mut peers = BTreeMap::<u32, Peer>::new();
    let mut ping = Instant::now();
    let mut pending = None::<Instant>;
    loop {
        control(c, identity, s, listener, Some(ws), &mut peers)?;
        if pending.is_some_and(|p| p.elapsed() > settings::PONG) {
            return Err("relay pong deadline".into());
        }
        if pending.is_none() && ping.elapsed() >= settings::PING {
            ws.send(Message::Text("homepool/1/bridge-ping".into()))
                .map_err(|_| "ping")?;
            pending = Some(Instant::now());
            ping = Instant::now();
        }
        let mut closed = vec![];
        for (&id, peer) in &mut peers {
            match peer.tick(c, s) {
                Ok(frames) => {
                    for frame in frames {
                        send(ws, 0, id, &frame)?;
                    }
                }
                Err(_) => closed.push(id),
            }
        }
        for id in closed {
            peers.remove(&id);
            send(ws, 2, id, &[])?;
        }
        match ws.read() {
            Ok(Message::Text(t)) if t == "homepool/1/bridge-pong" => {
                pending = None;
            }
            Ok(Message::Binary(b)) => {
                let (kind, id, data) = wire::unwrap(&b)?;
                match kind {
                    1 => {
                        if peers.len() < settings::SESSIONS && !peers.contains_key(&id) {
                            let binding =
                                u32::from_be_bytes(data.try_into().map_err(|_| "binding")?);
                            match Peer::new(c, s, identity, binding) {
                                Ok(peer) => {
                                    peers.insert(id, peer);
                                }
                                Err(_) => send(ws, 2, id, &[])?,
                            }
                        } else {
                            send(ws, 2, id, &[])?;
                        }
                    }
                    0 => {
                        let Some(peer) = peers.get_mut(&id) else {
                            continue;
                        };
                        peer.received += b.len() as u64;
                        let was_authenticated = peer.authenticated();
                        let result = peer.receive(c, s, data);
                        // Receipt is independent of Noise authentication; acknowledge before retiring a session.
                        let flush = peer.flush || !was_authenticated || result.as_ref().is_err();
                        if peer.received - peer.acked >= 65536 || flush {
                            send(ws, 8, id, &peer.received.to_be_bytes())?;
                            peer.acked = peer.received;
                        }
                        match result {
                            Ok(frames) => {
                                if !was_authenticated {
                                    register_if_enrolled(ws, s, peer)?;
                                }
                                for frame in frames {
                                    send(ws, 0, id, &frame)?;
                                }
                            }
                            Err(_) => {
                                peers.remove(&id);
                                send(ws, 2, id, &[])?;
                            }
                        }
                    }
                    2 | 3 => {
                        peers.remove(&id);
                    }
                    _ => return Err("relay control".into()),
                }
            }
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            _ => return Err("relay disconnected".into()),
        }
    }
}
fn register_if_enrolled(ws: &mut Socket, s: &State, peer: &Peer) -> Result<()> {
    if peer.authenticated()
        && peer.invitation.is_some()
        && let Some(d) = peer.device.as_ref().and_then(|id| s.devices.get(id))
    {
        token(ws, &d.token, 1, d.binding, 0)?;
    }
    Ok(())
}
fn control(
    c: &Config,
    identity: &Identity,
    s: &mut State,
    listener: &UnixListener,
    mut ws: Option<&mut Socket>,
    peers: &mut BTreeMap<u32, Peer>,
) -> Result<()> {
    let (mut stream, _) = match listener.accept() {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
        Err(_) => return Err("control accept".into()),
    };
    stream
        .set_read_timeout(Some(settings::TICK))
        .map_err(|_| "control timeout")?;
    stream
        .set_write_timeout(Some(settings::TICK))
        .map_err(|_| "control timeout")?;
    let mut bytes = vec![0; 1024];
    let n = stream.read(&mut bytes).map_err(|_| "control input")?;
    let m: Value = serde_json::from_slice(&bytes[..n]).map_err(|_| "control input")?;
    let response = match m["command"].as_str() {
        Some("invite") => {
            s.retired.extend(
                s.invitations
                    .values()
                    .filter(|i| i.expiry <= state::now() && i.device.is_none())
                    .map(|i| i.binding),
            );
            s.invitations
                .retain(|_, i| i.expiry > state::now() || i.device.is_some());
            if s.invitations
                .values()
                .filter(|i| i.expiry > state::now())
                .count()
                >= settings::MAX_INVITES
            {
                json!({"error":"invitation limit"})
            } else {
                let id = state::random(16)?;
                let i = state::Invitation {
                    id: id.clone(),
                    psk: state::random(32)?,
                    token: state::random(32)?,
                    binding: s.binding()?,
                    expiry: state::now() + settings::INVITATION_SECONDS,
                    device: None,
                };
                let url = format!(
                    "{}#route={}&token={}&bridge={}&invitation={}&psk={}",
                    c.page_origin, c.route, i.token, identity.public, i.id, i.psk
                );
                s.invitations.insert(id, i.clone());
                s.save(&c.state_dir)?;
                if let Some(ws) = &mut ws {
                    token(ws, &i.token, 0, i.binding, i.expiry)?;
                }
                json!({"link":url})
            }
        }
        Some("devices") => {
            json!({"devices":s.devices.values().map(|d|json!({"id":d.id,"fingerprint":d.key,"invitation":d.invitation})).collect::<Vec<_>>()})
        }
        Some("revoke") => {
            if let Some(d) = m["id"].as_str().and_then(|id| s.devices.remove(id)) {
                let mut bindings = vec![d.binding];
                for i in s
                    .invitations
                    .values_mut()
                    .filter(|i| i.device.as_deref() == Some(&d.id))
                {
                    i.expiry = state::now();
                    bindings.push(i.binding);
                }
                s.retired.extend(bindings.iter().copied());
                s.save(&c.state_dir)?;
                let ids = peers
                    .iter()
                    .filter(|(_, p)| p.device.as_deref() == Some(&d.id))
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>();
                for id in ids {
                    peers.remove(&id);
                    if let Some(ws) = &mut ws {
                        send(ws, 2, id, &[])?;
                    }
                }
                if let Some(ws) = &mut ws {
                    for binding in bindings {
                        send(ws, 5, 0, &binding.to_be_bytes())?;
                    }
                }
                json!({"revoked":true})
            } else {
                json!({"error":"unknown device"})
            }
        }
        _ => json!({"error":"command"}),
    };
    stream
        .write_all(
            serde_json::to_string(&response)
                .map_err(|_| "control response")?
                .as_bytes(),
        )
        .map_err(|_| "control response")?;
    Ok(())
}
pub fn command(dir: &Path, command: &str, id: Option<&str>) -> Result<()> {
    std::env::set_current_dir(dir).map_err(|_| "state directory")?;
    let mut stream = UnixStream::connect("control.sock").map_err(|_| "bridge not running")?;
    stream
        .set_read_timeout(Some(settings::PONG))
        .map_err(|_| "control timeout")?;
    let m = json!({"command":command,"id":id});
    stream
        .write_all(m.to_string().as_bytes())
        .map_err(|_| "control send")?;
    let mut response = String::new();
    stream
        .take(65536)
        .read_to_string(&mut response)
        .map_err(|_| "control response")?;
    println!("{response}");
    Ok(())
}
