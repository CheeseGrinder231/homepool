use std::{net::IpAddr, path::Path};

use crate::error::Error;

/// An explicit Docker endpoint. Local contexts and `DOCKER_HOST` cannot select it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Endpoint(String);

impl Endpoint {
    /// A local Unix socket, including a custom rootless socket. Requires Unix.
    pub fn unix(socket: impl AsRef<Path>) -> Result<Self, Error> {
        let socket = socket.as_ref();
        let path = socket.to_str().ok_or(Error::InvalidInput("socket path"))?;
        if !socket.is_absolute()
            || path.chars().any(char::is_control)
            || path.ends_with(char::is_whitespace)
            || path.contains("://")
        {
            return Err(Error::InvalidInput("socket path"));
        }
        // Docker treats Unix addresses as raw paths, not URL-encoded paths.
        Ok(Self(format!("unix://{path}")))
    }

    /// SSH host or SSH-config alias, with an optional port and remote socket.
    /// Authentication and login names come from the caller's SSH configuration.
    /// IPv6 addresses are supplied without brackets. Passwords are never accepted.
    /// Remote socket paths use only ASCII letters, digits, `/`, `-`, `.`, and `_`
    /// so older Docker clients cannot reinterpret them in a remote shell.
    pub fn ssh(host: &str, port: Option<u16>, socket: Option<&Path>) -> Result<Self, Error> {
        let host = if let Ok(address) = host.parse::<IpAddr>() {
            match address {
                IpAddr::V4(_) => host.to_owned(),
                IpAddr::V6(_) => format!("[{host}]"),
            }
        } else {
            if host.is_empty()
                || !host.as_bytes()[0].is_ascii_alphanumeric()
                || !host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
            {
                return Err(Error::InvalidInput("SSH host"));
            }
            host.to_owned()
        };
        if port == Some(0) {
            return Err(Error::InvalidInput("SSH port"));
        }
        let port = port.map(|value| format!(":{value}")).unwrap_or_default();
        let path = match socket {
            Some(socket) => {
                let path = socket.to_str().ok_or(Error::InvalidInput("socket path"))?;
                if !socket.is_absolute()
                    || !path
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"/-._".contains(&b))
                {
                    return Err(Error::InvalidInput("socket path"));
                }
                path.to_owned()
            }
            None => String::new(),
        };
        Ok(Self(format!("ssh://{host}{port}{path}")))
    }

    /// The Docker CLI's `--host` value. Contains no authentication material.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
