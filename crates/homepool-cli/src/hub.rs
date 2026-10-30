//! Serialized placement and lifecycle service. No durable hub-only ledger.
use crate::{
    inventory::{self, Inventory},
    private,
    protocol::{self, Budget, Message, Placement, Request, Response},
    settings::{self, CPU_QUANTUM, Configuration, HUB_POLL_INTERVAL, RESPONSE_MARGIN, Timing},
};
use homepool_adapter_docker::{Environment, Sandbox, SandboxSpec};
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
        net::UnixListener,
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
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
    let timing = Timing::load()?;
    let settle_until = Instant::now() + timing.operation;
    // Expose inventory immediately, but never trust an empty startup listing
    // for admission until one complete operation window has elapsed.
    let closed = Budget {
        deadline: Instant::now(),
        timing,
        cancelled: Arc::clone(&signals.cancelled),
    };
    let mut inventory = Inventory::rebuild(&config, BTreeMap::new(), &closed, false)?;
    while !signals.cancelled.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_nonblocking(true)
                    .map_err(|_| "could not configure API stream")?;
                let read_deadline = Instant::now() + timing.request;
                let mut response_deadline = read_deadline;
                let request = protocol::read_line(&mut stream, &signals.cancelled, read_deadline)
                    .and_then(|bytes| {
                        serde_json::from_slice::<Message>(&bytes)
                            .map_err(|_| "invalid API request".into())
                    });
                let result = request.and_then(|message| {
                    response_deadline = message.deadline(read_deadline)?;
                    let budget = Budget {
                        deadline: response_deadline
                            .checked_sub(RESPONSE_MARGIN)
                            .ok_or("invalid request deadline")?,
                        timing,
                        cancelled: Arc::clone(&signals.cancelled),
                    };
                    budget.check()?;
                    config = private::load(config_path)?;
                    // Release verifies its exact handle at the engine. Drain and
                    // add edit declarations; none need unrelated member probes.
                    if matches!(
                        &message.request,
                        Request::Inventory {} | Request::Place { .. } | Request::Remove { .. }
                    ) {
                        let previous = std::mem::take(&mut inventory.observed);
                        inventory = Inventory::rebuild(
                            &config,
                            previous,
                            &budget,
                            Instant::now() >= settle_until,
                        )?;
                    }
                    execute(
                        message.request,
                        config_path,
                        &mut config,
                        &mut inventory,
                        &budget,
                    )
                });
                let response = protocol::bytes(&Response::from_result(result)).or_else(|_| {
                    protocol::bytes(&Response::from_result(Err(
                        "API response exceeds size limit".into(),
                    )))
                })?;
                // A disconnected client never terminates the hub or retries work.
                let _ = protocol::write_line(
                    &mut stream,
                    &response,
                    &signals.cancelled,
                    response_deadline,
                );
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
    budget: &Budget,
) -> Result<Value, String> {
    budget.check()?;
    match request {
        Request::Inventory {} => Ok(inventory.render()),
        Request::Place { spec } => place(spec, config, inventory, budget),
        Request::Release { handle } => {
            let (member, container) = handle.split_once(':').ok_or("invalid sandbox handle")?;
            let member = MemberId(member.parse().map_err(|_| "invalid sandbox handle")?);
            // Stop/remove independently verify ownership on this exact member;
            // unrelated unavailable members cannot prevent reconciliation.
            let adapter = inventory::adapter(config, member, budget)?;
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
            budget.check()?;
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
            budget.check()?;
            private::save(path, config)?;
            Ok(json!({"member": members[0].id.0}))
        }
        Request::Remove { member } => {
            if inventory.settling {
                return Err(
                    "cannot remove a member while the hub is settling previous operations".into(),
                );
            }
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
            budget.check()?;
            private::save(path, config)?;
            Ok(json!({"removed": member}))
        }
    }
}

fn place(
    spec: Placement,
    config: &Configuration,
    inventory: &mut Inventory,
    budget: &Budget,
) -> Result<Value, String> {
    budget.check()?;
    if inventory.settling {
        return Err("hub is settling previous operations; admission remains closed".into());
    }
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
    if !spec.fresh && inventory.complete.values().any(|known| !known) {
        return Err("strict placement requires complete inventory of every member; fresh is only for a never-submitted identity".into());
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
    let adapter = inventory::adapter(config, allocation.member_id, budget)?;
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
        Err(error) => {
            // Preserve a known created handle even if subsequent lists fail.
            // A later successful inventory is the only reconciliation step.
            if let Some((container_id, sandbox_id)) = error.known_sandbox() {
                inventory
                    .observed
                    .entry(allocation.member_id)
                    .or_default()
                    .push(Sandbox {
                        container_id: container_id.into(),
                        sandbox_id: sandbox_id.into(),
                        state: "unknown".into(),
                        cpu_millicores: Some(allocation.budget.cpu_millicores),
                        memory_bytes: Some(allocation.budget.memory_bytes),
                    });
            }
            inventory.complete.insert(allocation.member_id, false);
            inventory
                .pool
                .set_state(allocation.member_id, MemberState::Unreachable)
                .map_err(|e| e.to_string())?;
            Err(format!("{error}; inspect inventory before retrying"))
        }
    }
}

fn check_socket(path: &Path) -> Result<(), String> {
    if !private::check(path, false)?.file_type().is_socket() {
        return Err("API path must be a Unix socket".into());
    }
    Ok(())
}

pub fn call(socket: &Path, request: Request) -> Result<Value, String> {
    let timing = settings::Timing::load()?;
    let deadline = Instant::now() + timing.request;
    let message = Message::new(request, timing.request)?;
    check_socket(socket)?;
    let cancelled = AtomicBool::new(false);
    let mut stream = protocol::connect(socket, &cancelled, deadline)?;
    protocol::write_line(
        &mut stream,
        &protocol::bytes(&message)?,
        &cancelled,
        deadline,
    )?;
    let response: Response =
        serde_json::from_slice(&protocol::read_line(&mut stream, &cancelled, deadline)?)
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
