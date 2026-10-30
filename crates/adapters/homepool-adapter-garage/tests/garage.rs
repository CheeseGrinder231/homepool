//! Opt-in Docker integration, also run explicitly by the CI test job.
use std::{
    ffi::OsString,
    fs,
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use homepool_adapter_garage::{Consumer, Garage};
use homepool_core::{Capabilities, Member, MemberId, MemberState, Tier};
use serde_json::{Value, json};

const GARAGE_IMAGE: &str =
    "dxflrs/garage:v2.1.0@sha256:4c9b34c113e61358466e83fd6e7d66e6d18657ede14b776eb78a93ee8da7cf6a";
const CLIENT_IMAGE: &str = "python:3.13.11-alpine3.22@sha256:2fd93799bfc6381d078a8f656a5f45d6092e5d11d16f55889b3d5cbfdc64f045";
// Isolated container namespace only; no published ports or host network.
const LOOPBACK: std::net::Ipv4Addr = std::net::Ipv4Addr::LOCALHOST;
const CLIENT: &str = include_str!("s3_client.py");

struct Installation {
    container: String,
    scratch: PathBuf,
}

impl Installation {
    fn new() -> Self {
        let scratch =
            PathBuf::from(std::env::var_os("TMPDIR").expect("set TMPDIR for integration"))
                .join(format!("garage-integration-{}", std::process::id()));
        fs::create_dir(&scratch).unwrap();
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
                "run",
                "-d",
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
        Self {
            container: String::from_utf8(output.stdout).unwrap().trim().into(),
            scratch,
        }
    }

    fn prefix(&self) -> Vec<OsString> {
        ["exec", "-i", &self.container, "/garage"]
            .map(OsString::from)
            .to_vec()
    }

    fn garage(&self) -> Garage {
        Garage::with_command("docker", self.prefix())
    }

    fn raw(&self, operation: &str, request: Value) -> Option<Value> {
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
    fn s3(&self, consumer: &Consumer, bucket: &str, action: &str) -> bool {
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
            "key_id": consumer.credentials.access_key_id(),
            "secret": consumer.credentials.secret_access_key()
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
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.container])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = fs::remove_dir_all(&self.scratch);
    }
}

#[test]
#[ignore = "requires Docker; CI runs this explicitly with TMPDIR set"]
fn single_node_portion_scoped_consumers_and_cross_container_s3() {
    let installation = Installation::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = installation.raw("GetClusterStatus", Value::Null) {
            break status;
        }
        assert!(Instant::now() < deadline, "Garage did not become ready");
        thread::sleep(Duration::from_millis(200));
    };
    let node_id = status["nodes"][0]["id"].as_str().expect("node ID");
    let member = Member {
        id: MemberId(1),
        tier: Tier::Core,
        state: MemberState::Active,
        capabilities: Capabilities {
            storage_bytes: 1_073_741_824,
            compute: None,
        },
    };
    let garage = installation.garage();
    garage.stage_portion(&member, node_id, "zone-a").unwrap();
    garage.apply_layout(1).unwrap();
    let layout = installation.raw("GetClusterLayout", Value::Null).unwrap();
    assert_eq!(
        layout["roles"][0]["capacity"],
        member.capabilities.storage_bytes
    );
    assert_eq!(layout["roles"][0]["tags"], json!(["homepool-member-1"]));

    let first = garage.create_consumer(1).unwrap();
    let second = garage.create_consumer(2).unwrap();
    assert!(
        garage.create_consumer(1).is_err(),
        "must not issue a second key"
    );
    for consumer in [&first, &second] {
        let key = installation
            .raw(
                "GetKeyInfo",
                json!({
                    "id": consumer.credentials.access_key_id(), "showSecretKey": false
                }),
            )
            .unwrap();
        assert_eq!(key["permissions"]["createBucket"], false);
        assert_eq!(key["buckets"].as_array().unwrap().len(), 1);
        assert_eq!(
            key["buckets"][0]["permissions"],
            json!({"read": true, "write": true, "owner": false})
        );
    }
    let keys = installation.raw("ListKeys", Value::Null).unwrap();
    assert_eq!(keys.as_array().unwrap().len(), 2);
    assert!(
        installation.s3(&first, &first.bucket, "put"),
        "first container must write"
    );
    assert!(
        installation.s3(&first, &first.bucket, "get"),
        "another container must read"
    );
    assert!(installation.s3(&second, &second.bucket, "put"));
    assert!(installation.s3(&first, &second.bucket, "deny-get"));
    assert!(installation.s3(&second, &first.bucket, "deny-get"));
    assert!(installation.s3(&first, &second.bucket, "deny-put"));
    assert!(installation.s3(&first, "unowned-consumer", "deny-create"));

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let usage = garage.storage_usage().unwrap();
        if usage.objects == 2 && usage.used_bytes == 2 * 24 {
            assert_eq!(usage.buckets, 2);
            assert_eq!(usage.unfinished_upload_bytes, 0);
            break;
        }
        assert!(Instant::now() < deadline, "usage counters did not converge");
        thread::sleep(Duration::from_millis(200));
    }
}
