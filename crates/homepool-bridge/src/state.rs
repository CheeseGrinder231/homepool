use crate::settings;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
pub type Result<T> = std::result::Result<T, String>;
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn unhex(s: &str) -> Result<Vec<u8>> {
    if !s.is_ascii() || !s.len().is_multiple_of(2) {
        return Err("hex".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| "hex".into()))
        .collect()
}
pub fn random(n: usize) -> Result<String> {
    let mut b = vec![0; n];
    getrandom::fill(&mut b).map_err(|_| "random")?;
    Ok(hex(&b))
}
pub fn hash(s: &str) -> Result<Vec<u8>> {
    Ok(Sha256::digest(unhex(s)?).to_vec())
}
pub fn private(path: &Path, directory: bool) -> Result<()> {
    let m = fs::symlink_metadata(path).map_err(|_| "private file missing")?;
    let uid = fs::metadata("/proc/self").map_err(|_| "owner")?.uid();
    if m.uid() != uid
        || m.mode() & 0o777 != if directory { 0o700 } else { 0o600 }
        || if directory { !m.is_dir() } else { !m.is_file() }
    {
        return Err("private permissions".into());
    }
    Ok(())
}
pub fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    private(path, false)?;
    let f = File::open(path).map_err(|_| "read")?;
    if f.metadata().map_err(|_| "read")?.len() > settings::STATE_BYTES {
        return Err("file bounds".into());
    }
    serde_json::from_reader(f).map_err(|_| "invalid private file".into())
}
pub fn atomic<T: Serialize>(dir: &Path, name: &str, value: &T) -> Result<()> {
    let temp = dir.join(format!(".{}", random(16)?));
    let result = (|| {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|_| "state create")?;
        serde_json::to_writer(&mut f, value).map_err(|_| "state encode")?;
        f.write_all(b"\n").map_err(|_| "state write")?;
        f.sync_all().map_err(|_| "state sync")?;
        fs::rename(&temp, dir.join(name)).map_err(|_| "state rename")?;
        File::open(dir)
            .and_then(|f| f.sync_all())
            .map_err(|_| "directory sync")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub private: String,
    pub public: String,
}
impl Identity {
    pub fn load(dir: &Path) -> Result<Self> {
        let key: Self = read(&dir.join("identity.json"))?;
        let private = unhex(&key.private)?;
        if private.len() != 32 || unhex(&key.public)?.len() != 32 {
            return Err("identity".into());
        }
        use snow::resolvers::CryptoResolver;
        let mut dh = snow::resolvers::DefaultResolver
            .resolve_dh(&snow::params::DHChoice::Curve25519)
            .ok_or("identity")?;
        dh.set(&private);
        if dh.pubkey() != unhex(&key.public)? {
            return Err("identity mismatch".into());
        }
        Ok(key)
    }
}
#[derive(Serialize, Deserialize, Clone)]
pub struct Device {
    pub id: String,
    pub key: String,
    pub token: String,
    pub binding: u32,
    pub invitation: String,
    pub enrolled: u64,
}
#[derive(Serialize, Deserialize, Clone)]
pub struct Invitation {
    pub id: String,
    pub psk: String,
    pub token: String,
    pub binding: u32,
    pub expiry: u64,
    pub device: Option<String>,
}
fn legacy_write_attempted() -> bool {
    true
}
#[derive(Serialize, Deserialize, Clone)]
pub struct Entry {
    // Legacy pending entries may already have reached PUT. Fail conservatively.
    #[serde(default = "legacy_write_attempted")]
    pub write_attempted: bool,
    #[serde(default)]
    pub settled_at: Option<u64>,
    #[serde(default)]
    pub committed_at: Option<u64>,
    pub device: String,
    pub id: String,
    pub key: String,
    pub name: String,
    pub mime: String,
    pub length: u64,
    pub status: String,
    pub hash: String,
    pub created: u64,
}
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct State {
    #[serde(default)]
    pub retired: Vec<u32>,
    pub devices: BTreeMap<String, Device>,
    pub invitations: BTreeMap<String, Invitation>,
    pub journal: BTreeMap<String, Entry>,
    pub binding: u32,
}
impl State {
    pub fn load(dir: &Path) -> Result<Self> {
        read(&dir.join("state.json"))
    }
    pub fn save(&self, dir: &Path) -> Result<()> {
        match atomic(dir, "state.json", self) {
            Ok(()) => Ok(()),
            Err(_) => {
                eprintln!("durable state write failed; stopping bridge");
                std::process::exit(1)
            }
        }
    }
    pub fn prune(&mut self) -> bool {
        let count = self.journal.len();
        self.journal.retain(|_, e| {
            e.status == "pending"
                || now().saturating_sub(e.committed_at.or(e.settled_at).unwrap_or(e.created))
                    < settings::RETENTION
        });
        self.journal.len() != count
    }
    pub fn binding(&mut self) -> Result<u32> {
        self.binding = self.binding.checked_add(1).ok_or("bindings exhausted")?;
        Ok(self.binding)
    }
    pub fn enroll(&mut self, dir: &Path, invitation: &str, key: String) -> Result<Device> {
        let invite = self.invitations.get(invitation).ok_or("invitation")?;
        if invite.expiry <= now()
            || invite.device.is_some()
            || self.devices.len() >= settings::MAX_DEVICES
        {
            return Err("invitation unavailable".into());
        }
        let device = Device {
            id: random(16)?,
            key,
            token: random(32)?,
            binding: self.binding()?,
            invitation: invitation.into(),
            enrolled: now(),
        };
        self.invitations.get_mut(invitation).unwrap().device = Some(device.id.clone());
        self.devices.insert(device.id.clone(), device.clone());
        self.save(dir)?;
        Ok(device)
    }
    pub fn reserve(&mut self, dir: &Path, entry: Entry) -> Result<String> {
        if self.prune() {
            self.save(dir)?;
        }
        let journal_key = format!("{}:{}", entry.device, entry.id);
        if entry.length > settings::MAX_FILE
            || self.journal.contains_key(&journal_key)
            || self.journal.len() >= settings::MAX_JOURNAL
        {
            return Err("upload refused".into());
        }
        let mut device = 0;
        let mut total = 0;
        let mut spool = crate::limits::download_bytes();
        for e in self.journal.values() {
            if e.status == "pending" {
                spool += e.length;
            }
            if e.created / 86400 == now() / 86400 && e.status != "failed" {
                total += e.length;
                if e.device == entry.device {
                    device += e.length;
                }
            }
        }
        if device + entry.length > settings::QUOTA
            || total + entry.length > settings::TOTAL_QUOTA
            || spool + entry.length > settings::SPOOL
        {
            return Err("quota".into());
        }
        self.journal.insert(journal_key.clone(), entry);
        self.save(dir)?;
        Ok(journal_key)
    }
}
pub fn init(dir: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(dir)
        .map_err(|_| "state directory must be new")?;
    let keys = snow::Builder::new("Noise_IK_25519_AESGCM_SHA256".parse().unwrap())
        .generate_keypair()
        .map_err(|_| "identity")?;
    atomic(
        dir,
        "identity.json",
        &Identity {
            private: hex(&keys.private),
            public: hex(&keys.public),
        },
    )?;
    State::default().save(dir)
}
use std::os::unix::fs::DirBuilderExt;
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub state_dir: PathBuf,
    pub relay_url: String,
    pub route: String,
    pub bridge_token_file: PathBuf,
    pub page_origin: String,
    pub s3_endpoint: String,
    pub bucket: String,
    pub region: String,
    pub credentials_file: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub version: u32,
    pub bucket: String,
    pub aws_access_key_id: String,
    pub aws_secret_access_key: String,
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let c: Self = read(path)?;
        private(&c.state_dir, true)?;
        if [&c.state_dir, &c.bridge_token_file, &c.credentials_file]
            .iter()
            .any(|p| !p.is_absolute())
        {
            return Err("private paths must be absolute".into());
        }
        homepool_protocol::prologue(&homepool_protocol::Mode::Reconnect, &c.route)?;
        let u = url::Url::parse(&c.relay_url).map_err(|_| "relay URL")?;
        let local = u.host_str().is_some_and(|h| {
            h == "localhost"
                || h.parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        if !(u.scheme() == "wss" || u.scheme() == "ws" && local)
            || u.query().is_some()
            || u.fragment().is_some()
            || !u.username().is_empty()
            || u.password().is_some()
            || u.path() != format!("/{}", c.route)
        {
            return Err("relay URL".into());
        }
        let page = url::Url::parse(&c.page_origin).map_err(|_| "page origin")?;
        if page.scheme() != "https"
            || page.path() != "/"
            || page.query().is_some()
            || page.fragment().is_some()
            || !page.username().is_empty()
            || page.password().is_some()
        {
            return Err("page origin".into());
        }
        Ok(c)
    }
    pub fn store(&self) -> Result<homepool_adapter_s3::Client> {
        let c: Credentials = read(&self.credentials_file)?;
        if c.version != 1 || c.bucket != self.bucket {
            return Err("credentials".into());
        }
        homepool_adapter_s3::Client::new(
            &self.s3_endpoint,
            self.bucket.clone(),
            self.region.clone(),
            c.aws_access_key_id,
            c.aws_secret_access_key,
        )
        .map_err(Into::into)
    }
    pub fn token(&self) -> Result<Vec<u8>> {
        private(&self.bridge_token_file, false)?;
        let mut s = String::new();
        File::open(&self.bridge_token_file)
            .map_err(|_| "token")?
            .take(66)
            .read_to_string(&mut s)
            .map_err(|_| "token")?;
        let b = unhex(s.trim())?;
        if b.len() != 32 {
            return Err("token".into());
        }
        Ok(b)
    }
}
