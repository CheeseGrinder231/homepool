//! Private files, atomic replacement, and the single writer lock.
use crate::settings::{self, Configuration};
use homepool_core::{MemberState, Tier};
use rustix::fs::{FlockOperation, OFlags, flock};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub fn check(path: &Path, directory: bool) -> Result<fs::Metadata, String> {
    let meta = fs::symlink_metadata(path).map_err(|_| "private path is unavailable")?;
    let expected = if directory { 0o700 } else { 0o600 };
    if meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o7777 != expected
        || meta.file_type().is_symlink()
        || (directory && !meta.is_dir())
    {
        return Err("private path must be owned by the current user with mode 0700 (directory) or 0600 (file)".into());
    }
    Ok(meta)
}

pub fn parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !parent.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|_| "could not create private directory")?;
    }
    check(parent, true)?;
    Ok(())
}

pub fn read(path: &Path) -> Result<String, String> {
    let meta = check(path, false)?;
    if !meta.is_file() {
        return Err("member file must be a regular file".into());
    }
    fs::read_to_string(path).map_err(|_| "could not read private member file".into())
}

pub fn load(path: &Path) -> Result<Configuration, String> {
    settings::parse_configuration(Path::new("members.toml"), &read(path)?)
        .map_err(|_| "invalid private member file".into())
}

pub fn lock(path: &Path) -> Result<File, String> {
    let name = path.file_name().ok_or("member file needs a filename")?;
    let mut lock_name = name.to_os_string();
    lock_name.push(".lock");
    let lock_path = path.with_file_name(lock_name);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(OFlags::NOFOLLOW.bits() as i32)
        .open(&lock_path)
        .map_err(|_| "could not open member lock")?;
    let meta = check(&lock_path, false)?;
    if !meta.is_file() {
        return Err("member lock must be a regular file".into());
    }
    flock(&file, FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| "another hub owns this member file")?;
    Ok(file)
}

pub fn encode(config: &Configuration) -> String {
    let mut out = String::from("version = 1\n");
    if config.pool.members().next().is_none() {
        out.push_str("members = []\n");
    }
    for member in config.pool.members() {
        let tier = match member.tier {
            Tier::Core => "core",
            Tier::Burst => "burst",
        };
        let state = match member.state {
            MemberState::Active => "active",
            MemberState::Draining => "draining",
            MemberState::Unreachable => "unreachable",
        };
        out.push_str(&format!("\n[[members]]\nid = {}\ntier = \"{tier}\"\nstate = \"{state}\"\n[members.capabilities]\nstorage_bytes = {}\n", member.id.0, member.capabilities.storage_bytes));
        if let Some(b) = member.capabilities.compute {
            out.push_str(&format!("[members.capabilities.compute]\ncpu_millicores = {}\nmemory_bytes = {}\nsandbox_slots = {}\n", b.cpu_millicores, b.memory_bytes, b.sandbox_slots));
        }
        if let Some(profiles) = config.profiles.get(&member.id) {
            // Serialize a fully qualified table tree so mount-array headers stay
            // inside their profile when attached to the current member.
            let tree = std::collections::BTreeMap::from([(
                "members",
                std::collections::BTreeMap::from([(
                    "capabilities",
                    std::collections::BTreeMap::from([(
                        "compute",
                        std::collections::BTreeMap::from([("profiles", profiles)]),
                    )]),
                )]),
            )]);
            out.push_str(&toml::to_string(&tree).expect("typed profile serialization"));
        }
        if let Some(endpoint) = config.docker.get(&member.id) {
            out.push_str("[members.docker]\n");
            // String escaping is shared with TOML's basic-string syntax.
            let quoted = |s: &str| serde_json::to_string(s).expect("string serialization");
            match endpoint {
                settings::DockerEndpoint::Unix { socket } => {
                    out.push_str(&format!("socket = {}\n", quoted(socket)))
                }
                settings::DockerEndpoint::Ssh {
                    host,
                    port,
                    remote_socket,
                } => {
                    out.push_str(&format!("host = {}\n", quoted(host)));
                    if let Some(port) = port {
                        out.push_str(&format!("port = {port}\n"));
                    }
                    if let Some(socket) = remote_socket {
                        out.push_str(&format!("remote_socket = {}\n", quoted(socket)));
                    }
                }
            }
        }
    }
    out
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn save(path: &Path, config: &Configuration) -> Result<(), String> {
    let source = encode(config);
    settings::parse_configuration(Path::new("members.toml"), &source)
        .map_err(|_| "member update failed validation")?;
    check(path, false)?;
    let name = path.file_name().ok_or("member file needs a filename")?;
    let mut scratch_name = name.to_os_string();
    scratch_name.push(format!(".{}.new", std::process::id()));
    let scratch = path.with_file_name(scratch_name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&scratch)
        .map_err(|_| "could not create private replacement")?;
    let _cleanup = Temporary(scratch.clone());
    file.write_all(source.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|_| "could not persist member update")?;
    fs::rename(&scratch, path).map_err(|_| "could not replace member file")?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|_| "member file replaced but directory sync failed")?;
    Ok(())
}

/// A reserved new private file. Never follows or replaces an existing entry.
/// Uncommitted files are removed on failure, including partially written secrets.
pub struct NewFile {
    file: File,
    path: PathBuf,
    persisted: bool,
}

impl NewFile {
    pub fn create(path: &Path) -> Result<Self, String> {
        parent(path)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(
                |_| "could not create new private credentials file; destination must not exist",
            )?;
        let new = Self {
            file,
            path: path.to_owned(),
            persisted: false,
        };
        // An unusually restrictive umask may remove owner bits. The inode was
        // created with at most 0600, and is never widened beyond that mode.
        new.file
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| "could not secure credentials file")?;
        Ok(new)
    }

    pub fn persist(&mut self, contents: &[u8]) -> Result<(), std::io::Error> {
        self.file.write_all(contents)?;
        self.file.write_all(b"\n")?;
        self.file.sync_all()?;
        self.persisted = true;
        Ok(())
    }
}

impl Drop for NewFile {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Read a bounded regular private file without following a replacement symlink.
pub fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let load = || {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        check(parent, true).ok()?;
        let before = check(path, false).ok()?;
        if !before.is_file() {
            return None;
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
            .open(path)
            .ok()?;
        let after = file.metadata().ok()?;
        if before.ino() != after.ino() || before.dev() != after.dev() {
            return None;
        }
        let mut bytes = Vec::new();
        file.take((limit + 1) as u64).read_to_end(&mut bytes).ok()?;
        (bytes.len() <= limit).then_some(bytes)
    };
    load().ok_or_else(|| "invalid private file".into())
}
