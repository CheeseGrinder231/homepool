//! Shared isolated Garage installation for adapter and shipped CLI integration tests.
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs,
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
};

const GARAGE_IMAGE: &str =
    "dxflrs/garage:v2.1.0@sha256:4c9b34c113e61358466e83fd6e7d66e6d18657ede14b776eb78a93ee8da7cf6a";
const CLIENT_IMAGE: &str = "python:3.13.11-alpine3.22@sha256:2fd93799bfc6381d078a8f656a5f45d6092e5d11d16f55889b3d5cbfdc64f045";
// Isolated container namespace only; no published ports or host network.
const LOOPBACK: std::net::Ipv4Addr = std::net::Ipv4Addr::LOCALHOST;
const CLIENT: &str = include_str!("../s3_client.py");

pub struct Installation {
    pub container: String,
    pub scratch: PathBuf,
}

impl Installation {
    pub fn new() -> Self {
        let scratch =
            PathBuf::from(std::env::var_os("TMPDIR").expect("set TMPDIR for integration"))
                .join(format!("garage-integration-{}", std::process::id()));
        fs::create_dir(&scratch).unwrap();
        let mut installation = Self {
            container: String::new(),
            scratch: scratch.clone(),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut random = [0_u8; 32];
        fs::File::open("/dev/urandom")
            .unwrap()
            .read_exact(&mut random)
            .unwrap();
        let secret: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let config = scratch.join("garage.toml");
        fs::write(
            &config,
            format!(
                r#"
metadata_dir = "/data/meta"
data_dir = "/data/blocks"
db_engine = "lmdb"
replication_factor = 1
rpc_bind_addr = "{LOOPBACK}:3901"
rpc_public_addr = "{LOOPBACK}:3901"
rpc_secret = "{secret}"
[s3_api]
s3_region = "homepool"
api_bind_addr = "{LOOPBACK}:3900"
"#
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mount = format!(
            "type=bind,src={},dst=/etc/garage.toml,readonly",
            config.display()
        );
        let output = Command::new("docker")
            .args([
                "create",
                "--network",
                "none",
                "--mount",
                &mount,
                GARAGE_IMAGE,
            ])
            .output()
            .expect("Docker must be available");
        assert!(
            output.status.success(),
            "could not start isolated Garage container"
        );
        installation.container = String::from_utf8(output.stdout).unwrap().trim().into();
        let started = Command::new("docker")
            .args(["start", &installation.container])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            started.success(),
            "could not start isolated Garage container"
        );
        installation
    }

    fn prefix(&self) -> Vec<OsString> {
        ["exec", "-i", &self.container, "/garage"]
            .map(OsString::from)
            .to_vec()
    }

    pub fn raw(&self, operation: &str, request: Value) -> Option<Value> {
        let mut child = Command::new("docker")
            .args(self.prefix())
            .args(["json-api", operation, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let _ = child
            .stdin
            .take()?
            .write_all(&serde_json::to_vec(&request).ok()?);
        let output = child.wait_with_output().ok()?;
        output
            .status
            .success()
            .then(|| serde_json::from_slice(&output.stdout).ok())
            .flatten()
    }

    // Each call starts a fresh client container. Credentials travel only on stdin.
    pub fn s3(&self, key_id: &str, secret: &str, bucket: &str, action: &str) -> bool {
        let mut child = Command::new("docker")
            .args([
                "run",
                "--rm",
                "-i",
                "--network",
                &format!("container:{}", self.container),
                CLIENT_IMAGE,
                "python",
                "-c",
                CLIENT,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let request = json!({
            "host": LOOPBACK, "bucket": bucket, "action": action,
            "key_id": key_id,
            "secret": secret
        });
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&request).unwrap())
            .unwrap();
        child.wait().unwrap().success()
    }
}

impl Drop for Installation {
    fn drop(&mut self) {
        if !self.container.is_empty() {
            let _ = Command::new("docker")
                .args(["rm", "-f", &self.container])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = fs::remove_dir_all(&self.scratch);
    }
}
