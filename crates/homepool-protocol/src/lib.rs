//! Pure wire protocol for remote image sessions. No I/O or authorization policy.
pub mod wire;

pub enum Mode {
    Enroll,
    Reconnect,
}
impl Mode {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Enroll => "enroll",
            Self::Reconnect => "reconnect",
        }
    }
}
pub fn prologue(mode: &Mode, route: &str) -> Result<Vec<u8>, String> {
    if route.is_empty()
        || route.len() > 64
        || !route
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("route".into());
    }
    Ok(format!("homepool/1/{}/{route}", mode.name()).into_bytes())
}
enum State {
    Handshake(Box<snow::HandshakeState>),
    Transport(snow::TransportState),
    Closed,
}
pub struct Session {
    state: State,
}
impl Session {
    pub fn new(
        mode: Mode,
        initiator: bool,
        route: &str,
        private: &[u8],
        remote: Option<&[u8]>,
        psk: Option<&[u8]>,
    ) -> Result<Self, String> {
        let name = match mode {
            Mode::Enroll => "Noise_XKpsk3_25519_AESGCM_SHA256",
            Mode::Reconnect => "Noise_IK_25519_AESGCM_SHA256",
        };
        let prologue = prologue(&mode, route)?;
        let mut b = snow::Builder::new(name.parse().map_err(|_| "pattern")?)
            .local_private_key(private)
            .map_err(|_| "key")?
            .prologue(&prologue)
            .map_err(|_| "prologue")?;
        if let Some(k) = remote {
            b = b.remote_public_key(k).map_err(|_| "remote")?;
        }
        if matches!(mode, Mode::Enroll) {
            let k = psk.ok_or("PSK")?;
            if k.len() != 32 {
                return Err("PSK".into());
            }
            b = b
                .psk(3, k.try_into().map_err(|_| "PSK")?)
                .map_err(|_| "PSK")?;
        }
        let h = if initiator {
            b.build_initiator()
        } else {
            b.build_responder()
        }
        .map_err(|_| "handshake")?;
        Ok(Self {
            state: State::Handshake(Box::new(h)),
        })
    }
    pub fn remote_static(&self) -> Option<&[u8]> {
        match &self.state {
            State::Handshake(h) => h.get_remote_static(),
            State::Transport(t) => t.get_remote_static(),
            _ => None,
        }
    }
    pub fn message(
        &mut self,
        read: bool,
        handshake: bool,
        input: &[u8],
    ) -> Result<Vec<u8>, String> {
        let mut state = std::mem::replace(&mut self.state, State::Closed);
        let mut out = vec![0; 65535];
        if input.len() > if read { 65535 } else { 65519 } {
            return Err("size".into());
        }
        let n = match &mut state {
            State::Handshake(h) if handshake => {
                if read {
                    let n = h.read_message(input, &mut out).map_err(|_| "handshake")?;
                    if n != 0 {
                        return Err("handshake payload".into());
                    }
                    n
                } else {
                    if !input.is_empty() {
                        return Err("handshake payload".into());
                    }
                    h.write_message(input, &mut out).map_err(|_| "handshake")?
                }
            }
            State::Transport(t) if !handshake => if read {
                t.read_message(input, &mut out)
            } else {
                t.write_message(input, &mut out)
            }
            .map_err(|_| "transport")?,
            _ => return Err("state".into()),
        };
        self.state = match state {
            State::Handshake(h) if h.is_handshake_finished() => {
                State::Transport(h.into_transport_mode().map_err(|_| "split")?)
            }
            s => s,
        };
        out.truncate(n);
        Ok(out)
    }
}
