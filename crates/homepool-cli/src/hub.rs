//! Serialized placement and lifecycle service. No durable hub-only ledger.
use crate::{
    inventory::{self, Inventory},
    private,
    protocol::{self, Placement, Request, Response},
    settings::{self, CPU_QUANTUM, Configuration, HUB_POLL_INTERVAL},
};
use homepool_adapter_docker::{Environment, SandboxSpec};
use homepool_core::{MemberId, MemberState, SandboxRequest};
use serde_json::{Value, json};
use signal_hook::{
    SigId,
    consts::{SIGINT, SIGTERM},
    flag,
    low_level::unregister,
};
use std::{
    collections::BTreeMap,
    fs, io,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

struct Signals {
    ids: Vec<SigId>,
    cancelled: Arc<AtomicBool>,
    default_action: Arc<AtomicBool>,
}
impl Signals {
    fn new() -> Result<Self, String> {
        let mut signals = Self {
            ids: Vec::new(),
            cancelled: Arc::new(AtomicBool::new(false)),
            default_action: Arc::new(AtomicBool::new(true)),
        };
        for signal in [SIGINT, SIGTERM] {
            // unregister does not restore the OS handler in signal-hook. Keep
            // a conditional default action installed for partial setup and the
            // entire time after serving, including possibly blocked diagnostics.
            flag::register_conditional_default(signal, Arc::clone(&signals.default_action))
                .map_err(|_| "could not register hub termination fallback")?;
            signals.ids.push(
                flag::register(signal, Arc::clone(&signals.cancelled))
                    .map_err(|_| "could not register hub cancellation")?,
            );
        }
        signals.default_action.store(false, Ordering::SeqCst);
        Ok(signals)
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        self.default_action.store(true, Ordering::SeqCst);
        for id in &self.ids {
            unregister(*id);
        }
    }
}

struct SocketFile {
    path: PathBuf,
    inode: u64,
    device: u64,
}
impl Drop for SocketFile {
    fn drop(&mut self) {
        if let Ok(meta) = fs::symlink_metadata(&self.path)
            && meta.ino() == self.inode
            && meta.dev() == self.device
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn serve(config_path: &Path, socket: &Path) -> Result<(), String> {
    // Validate regular private files before installing handlers: no blocked FIFO
    // reads in a scope that suppresses default signal termination.
    private::parent(config_path)?;
    let _lock = private::lock(config_path)?;
    let mut config = private::load(config_path)?;
    private::parent(socket)?;
    // Restrictive umask means the socket is owner-only from the moment of bind.
    let previous = rustix::process::umask(rustix::fs::Mode::from_bits_truncate(0o177));
    let bound = UnixListener::bind(socket);
    rustix::process::umask(previous);
    let listener = bound.map_err(
        |_| "could not bind hub socket; an existing path is never removed automatically",
    )?;
    let meta = fs::symlink_metadata(socket).map_err(|_| "could not inspect hub socket")?;
    let _socket_file = SocketFile {
        path: socket.to_owned(),
        inode: meta.ino(),
        device: meta.dev(),
    };
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))
        .map_err(|_| "could not restrict hub socket")?;
    check_socket(socket)?;
    listener
        .set_nonblocking(true)
        .map_err(|_| "could not configure hub socket")?;
    let signals = Signals::new()?;
    let mut inventory = Inventory::rebuild(&config, BTreeMap::new(), &signals.cancelled)?;
    while !signals.cancelled.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_nonblocking(true)
                    .map_err(|_| "could not configure API stream")?;
                let request =
                    protocol::read_line(&mut stream, &signals.cancelled).and_then(|bytes| {
                        serde_json::from_slice::<Request>(&bytes)
                            .map_err(|_| "invalid API request".into())
                    });
                let result = request.and_then(|request| {
                    config = private::load(config_path)?;
                    let previous = std::mem::take(&mut inventory.observed);
                    inventory = Inventory::rebuild(&config, previous, &signals.cancelled)?;
                    execute(
                        request,
                        config_path,
                        &mut config,
                        &mut inventory,
                        &signals.cancelled,
                    )
                });
                let response = protocol::bytes(&Response::from_result(result)).or_else(|_| {
                    protocol::bytes(&Response::from_result(Err(
                        "API response exceeds size limit".into(),
                    )))
                })?;
                // A disconnected client never terminates the hub or retries work.
                let _ = protocol::write_line(&mut stream, &response, &signals.cancelled);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(HUB_POLL_INTERVAL)
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err("hub accept failed".into()),
        }
    }
    Ok(())
}

fn execute(
    request: Request,
    path: &Path,
    config: &mut Configuration,
    inventory: &mut Inventory,
    cancelled: &Arc<AtomicBool>,
) -> Result<Value, String> {
    match request {
        Request::Inventory => Ok(inventory.render()),
        Request::Place { spec } => place(spec, config, inventory, cancelled),
        Request::Release { handle } => {
            let (member, container) = handle.split_once(':').ok_or("invalid sandbox handle")?;
            let member = MemberId(member.parse().map_err(|_| "invalid sandbox handle")?);
            // Retain mappings even when a shrink prevented ledger imports.
            if !inventory
                .observed
                .get(&member)
                .is_some_and(|items| items.iter().any(|s| s.container_id == container))
            {
                return Err(
                    "handle is absent from observed inventory; reconcile before retrying".into(),
                );
            }
            let adapter = inventory::adapter(config, member, cancelled)?;
            adapter
                .stop(container)
                .and_then(|_| adapter.remove(container))
                .map_err(|_| "release outcome uncertain; inspect inventory before retrying")?;
            Ok(json!({"released": handle}))
        }
        Request::Drain { member } => {
            config
                .pool
                .set_state(MemberId(member), MemberState::Draining)
                .map_err(|e| e.to_string())?;
            private::save(path, config)?;
            Ok(json!({"member": member, "state": "draining"}))
        }
        Request::Add { document } => {
            let addition = settings::parse_configuration(Path::new("members.toml"), &document)
                .map_err(|_| "invalid member addition")?;
            let members: Vec<_> = addition.pool.members().cloned().collect();
            if members.len() != 1 {
                return Err("add requires exactly one member".into());
            }
            config
                .pool
                .add_member(members[0].clone())
                .map_err(|e| e.to_string())?;
            config.docker.extend(addition.docker);
            private::save(path, config)?;
            Ok(json!({"member": members[0].id.0}))
        }
        Request::Remove { member } => {
            let id = MemberId(member);
            if config.pool.member(id).map_err(|e| e.to_string())?.state != MemberState::Draining {
                return Err("drain member before removing it".into());
            }
            if !inventory.complete.get(&id).copied().unwrap_or(false)
                || !inventory.observed[&id].is_empty()
            {
                return Err("remove requires complete empty inventory".into());
            }
            let mut pool = homepool_core::Pool::default();
            for m in config.pool.members().filter(|m| m.id != id) {
                pool.add_member(m.clone()).map_err(|e| e.to_string())?;
            }
            config.pool = pool;
            config.docker.remove(&id);
            private::save(path, config)?;
            Ok(json!({"removed": member}))
        }
    }
}

fn place(
    spec: Placement,
    config: &Configuration,
    inventory: &mut Inventory,
    cancelled: &Arc<AtomicBool>,
) -> Result<Value, String> {
    if spec.cpu_millicores == 0 || !spec.cpu_millicores.is_multiple_of(CPU_QUANTUM) {
        return Err("CPU request must be positive and a multiple of 10 millicores".into());
    }
    if inventory
        .observed
        .values()
        .flatten()
        .any(|s| s.sandbox_id == spec.sandbox_id)
    {
        return Err(
            "sandbox identity already observed; reconcile existing handles before retrying".into(),
        );
    }
    let mut environment = Environment::default();
    for (key, value) in spec.environment {
        environment
            .insert(&key, &value)
            .map_err(|e| e.to_string())?;
    }
    let allocation = inventory
        .pool
        .reserve(&SandboxRequest {
            cpu_millicores: spec.cpu_millicores,
            memory_bytes: spec.memory_bytes,
            preferences: spec.preferences.into_iter().map(MemberId).collect(),
        })
        .map_err(|e| e.to_string())?;
    let adapter = inventory::adapter(config, allocation.member_id, cancelled)?;
    let result = adapter.start(&SandboxSpec {
        sandbox_id: spec.sandbox_id,
        image: spec.image,
        cpu_millicores: spec.cpu_millicores,
        memory_bytes: spec.memory_bytes,
        mounts: Vec::new(),
        environment,
        command: spec.command,
    });
    match result {
        Ok(sandbox) => {
            let handle = inventory::handle(allocation.member_id, &sandbox.container_id);
            inventory
                .observed
                .entry(allocation.member_id)
                .or_default()
                .push(sandbox);
            Ok(json!({"member": allocation.member_id.0, "handle": handle}))
        }
        Err(_) => {
            // Never retry a mutation. Reconcile before serving another request.
            let previous = std::mem::take(&mut inventory.observed);
            *inventory = Inventory::rebuild(config, previous, cancelled)?;
            Err("start failed; outcome may be uncertain; inspect inventory before retrying".into())
        }
    }
}

fn check_socket(path: &Path) -> Result<(), String> {
    if !private::check(path, false)?.file_type().is_socket() {
        return Err("API path must be a Unix socket".into());
    }
    Ok(())
}

pub fn call(socket: &Path, request: &Request) -> Result<Value, String> {
    check_socket(socket)?;
    let mut stream = UnixStream::connect(socket).map_err(|_| "could not connect to hub socket")?;
    stream
        .set_nonblocking(true)
        .map_err(|_| "could not configure API stream")?;
    let cancelled = AtomicBool::new(false);
    protocol::write_line(&mut stream, &protocol::bytes(request)?, &cancelled)?;
    let response: Response = serde_json::from_slice(&protocol::read_line(&mut stream, &cancelled)?)
        .map_err(|_| "invalid hub response")?;
    if response.version != 1 {
        return Err("unsupported hub API version".into());
    }
    if let Some(error) = response.error {
        return Err(error);
    }
    response.result.ok_or("missing hub result".into())
}

#[cfg(test)]
mod tests {
    use super::Signals;
    use std::{os::unix::process::ExitStatusExt, process::Command};
    #[test]
    fn termination_uses_default_action_after_cancellation_scope() {
        if let Ok(signal) = std::env::var("HOMEPOOL_SIGNAL_SCOPE_CHILD") {
            drop(Signals::new().unwrap());
            let signal = match signal.as_str() {
                "int" => rustix::process::Signal::INT,
                _ => rustix::process::Signal::TERM,
            };
            rustix::process::kill_process(rustix::process::getpid(), signal).unwrap();
            panic!("termination signal was swallowed after scope exit");
        }
        for (name, number) in [
            ("int", signal_hook::consts::SIGINT),
            ("term", signal_hook::consts::SIGTERM),
        ] {
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "hub::tests::termination_uses_default_action_after_cancellation_scope",
                ])
                .env("HOMEPOOL_SIGNAL_SCOPE_CHILD", name)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert_eq!(status.signal(), Some(number));
        }
    }
}
