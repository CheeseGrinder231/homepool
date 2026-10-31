#![cfg(target_os = "linux")]
use rustix::process::{Pid, Signal, kill_process};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    hub: Option<Child>,
    real: bool,
    created: RefCell<Vec<(u64, String)>>,
    cpu_ticks: u64,
    peak_kib: u64,
    operation_ms: u64,
    request_ms: u64,
}
impl Fixture {
    fn new(real: bool) -> Self {
        let root = std::env::temp_dir().join(format!(
            "hub-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let fixture = Self {
            root,
            hub: None,
            real,
            created: RefCell::new(Vec::new()),
            cpu_ticks: 0,
            peak_kib: 0,
            operation_ms: if real { 5000 } else { 1000 },
            request_ms: if real { 30000 } else { 6000 },
        };
        if !real {
            fs::write(
                fixture.root.join("docker"),
                include_str!("support/docker.py"),
            )
            .unwrap();
            fs::set_permissions(
                fixture.root.join("docker"),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        fixture.write("members.toml", &fixture.member(1, "active", 1000, 3));
        fixture
    }
    fn member(&self, id: u64, state: &str, cpu: u64, slots: u32) -> String {
        let socket = if self.real {
            std::env::var("HOMEPOOL_TEST_DOCKER_SOCKET")
                .unwrap_or_else(|_| "/var/run/docker.sock".into())
        } else {
            format!("/run/member/{id}.sock")
        };
        format!(
            "version = 1\n[[members]]\nid = {id}\ntier = \"core\"\nstate = \"{state}\"\n[members.capabilities.compute]\ncpu_millicores = {cpu}\nmemory_bytes = 268435456\nsandbox_slots = {slots}\n[members.docker]\nsocket = {}\n",
            serde_json::to_string(&socket).unwrap()
        )
    }
    fn write(&self, name: &str, source: &str) {
        fs::write(self.root.join(name), source).unwrap();
        fs::set_permissions(self.root.join(name), fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_homepool"));
        c.current_dir(&self.root)
            .env_remove("HOMEPOOL_CONFIG")
            .env_remove("HOMEPOOL_SOCKET")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_RUNTIME_DIR")
            .env(
                "HOMEPOOL_OPERATION_TIMEOUT_MS",
                self.operation_ms.to_string(),
            )
            .env("HOMEPOOL_REQUEST_TIMEOUT_MS", self.request_ms.to_string());
        if !self.real {
            c.env(
                "PATH",
                format!("{}:{}", self.root.display(), std::env::var("PATH").unwrap()),
            )
            .env("HOMEPOOL_FAKE_STATE", &self.root);
        }
        c
    }
    fn spawn(&mut self) {
        self.hub = Some(
            self.command()
                .args(["hub", "--config", "members.toml", "--socket", "hub.sock"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
    }
    fn start(&mut self) {
        self.spawn();
        self.wait_settled();
    }
    fn wait_settled(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                self.hub.as_mut().unwrap().try_wait().unwrap().is_none(),
                "hub exited during startup"
            );
            if self.root.join("hub.sock").exists() {
                let output = self.run(&["inventory", "--json"], None);
                if output.status.success()
                    && serde_json::from_slice::<Value>(&output.stdout).unwrap()["settling"] == false
                {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "hub startup watchdog");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn stop(&mut self, signal: Signal) {
        let mut child = self.hub.take().unwrap();
        let (ticks, _, peak) = process_sample(child.id());
        self.cpu_ticks += ticks;
        self.peak_kib = self.peak_kib.max(peak);
        kill_process(Pid::from_raw(child.id() as i32).unwrap(), signal).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if Instant::now() >= until {
                let _ = child.kill();
                let _ = child.wait();
                panic!("shutdown watchdog");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!self.root.join("hub.sock").exists());
    }
    fn run(&self, args: &[&str], input: Option<Value>) -> Output {
        let mut child = self
            .command()
            .args(["--socket", "hub.sock"])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(input) = input {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(serde_json::to_string(&input).unwrap().as_bytes())
                .unwrap();
        }
        child.stdin.take();
        child.wait_with_output().unwrap()
    }
    fn ok(&self, args: &[&str], input: Option<Value>) -> Value {
        let out = self.run(args, input);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn place(&self, name: &str, cpu: u64, memory: u64) -> Value {
        let result = self.ok(&["place", "--json"], Some(json!({"sandbox_id": name, "image": "alpine:3.22", "cpu_millicores": cpu, "memory_bytes": memory, "command": ["sh", "-c", "trap 'exit 0' TERM; while :; do sleep 1 & wait $!; done"]})));
        if self.real {
            let (member, container) = result["handle"].as_str().unwrap().split_once(':').unwrap();
            self.created
                .borrow_mut()
                .push((member.parse().unwrap(), container.into()));
        }
        result
    }
    fn inventory(&self) -> Value {
        self.ok(&["inventory", "--json"], None)
    }
    fn release(&self, placed: &Value) {
        self.ok(&["release", placed["handle"].as_str().unwrap()], None);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut hub) = self.hub.take() {
            let _ = hub.kill();
            let _ = hub.wait();
        }
        if self.real {
            // Only exact handles returned by this fixture are eligible for cleanup.
            for (member, container) in self.created.borrow().iter() {
                let socket = std::env::var("HOMEPOOL_TEST_DOCKER_SOCKET")
                    .unwrap_or_else(|_| "/var/run/docker.sock".into());
                let engine = homepool_adapter_docker::Docker::new(
                    homepool_adapter_docker::Endpoint::unix(socket).unwrap(),
                    homepool_core::MemberId(*member),
                );
                let _ = engine.stop(container);
                let _ = engine.remove(container);
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn restart_and_drain(real: bool) {
    let mut f = Fixture::new(real);
    let measurement_started = Instant::now();
    f.start();
    let meta = fs::metadata(f.root.join("hub.sock")).unwrap();
    assert_eq!(meta.mode() & 0o777, 0o600);
    let first = f.place("restart-small", 200, 33554432);
    let second = f.place("restart-large", 700, 67108864);
    let before = f.inventory();
    assert_eq!(
        before["members"][0]["sandboxes"].as_array().unwrap().len(),
        2
    );
    assert_eq!(before["members"][0]["free"]["cpu_millicores"], 100);
    f.stop(Signal::TERM);
    f.start(); // A different process, with no old in-memory mapping.
    assert_eq!(f.inventory(), before);
    let duplicate = f.run(&["place", "--json"], Some(json!({"sandbox_id": "restart-small", "image": "alpine:3.22", "cpu_millicores": 100, "memory_bytes": 33554432})));
    assert!(!duplicate.status.success());
    f.ok(&["drain", "1"], None);
    f.stop(Signal::INT);
    f.start();
    assert_eq!(f.inventory()["members"][0]["state"], "draining");
    assert!(!f.run(&["remove", "1"], None).status.success());
    assert!(!f.run(&["place", "--json"], Some(json!({"sandbox_id": "blocked", "image": "alpine:3.22", "cpu_millicores": 100, "memory_bytes": 33554432}))).status.success());
    f.release(&first);
    f.release(&second);
    f.ok(&["remove", "1"], None);
    assert!(f.inventory()["members"].as_array().unwrap().is_empty());
    f.write("addition.toml", &f.member(2, "active", 1000, 3));
    f.ok(&["add", "addition.toml"], None);
    let third = f.place("added-member", 300, 33554432);
    assert_eq!(third["member"], 2);
    f.release(&third);
    assert_eq!(
        fs::metadata(f.root.join("members.toml")).unwrap().mode() & 0o777,
        0o600
    );
    let (ticks, rss, peak) = process_sample(f.hub.as_ref().unwrap().id());
    let hz = String::from_utf8(
        Command::new("getconf")
            .arg("CLK_TCK")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .parse::<f64>()
    .unwrap();
    println!(
        "hub resource sample (real_engine={real}): total hub CPU seconds={:.3}; latest RSS KiB={rss}; peak RSS KiB={}; full scenario wall seconds={:.3}",
        (f.cpu_ticks + ticks) as f64 / hz,
        f.peak_kib.max(peak),
        measurement_started.elapsed().as_secs_f64()
    );
    f.stop(Signal::TERM);
}

#[test]
fn process_restart_and_drain() {
    restart_and_drain(false);
}
#[test]
#[ignore = "requires a disposable Docker engine and alpine:3.22"]
fn engine_restart_and_drain() {
    restart_and_drain(true);
}

#[test]
fn incomplete_inventory_and_budget_shrink_recover_without_losing_handles() {
    let mut f = Fixture::new(false);
    f.start();
    let first = f.place("shrink-small", 200, 33554432);
    let second = f.place("shrink-large", 700, 67108864);
    for marker in ["unavailable", "partial"] {
        f.write(marker, "");
        let inventory = f.inventory();
        assert_eq!(inventory["members"][0]["inventory_complete"], false);
        assert_eq!(
            inventory["members"][0]["sandboxes"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(inventory["members"][0]["free"]["sandbox_slots"], 0);
        fs::remove_file(f.root.join(marker)).unwrap();
    }
    f.stop(Signal::TERM);
    f.write("members.toml", &f.member(1, "active", 500, 3));
    f.start();
    let shrunk = f.inventory();
    assert_eq!(shrunk["members"][0]["inventory_complete"], false);
    assert_eq!(
        shrunk["members"][0]["sandboxes"].as_array().unwrap().len(),
        2
    );
    f.release(&second);
    assert_eq!(f.inventory()["members"][0]["free"]["cpu_millicores"], 300);
    f.release(&first);
    f.stop(Signal::TERM);
}

#[test]
fn uncertain_start_is_reconciled_before_another_placement() {
    let mut f = Fixture::new(false);
    f.start();
    f.write("uncertain", "");
    f.write("fail-cleanup", "");
    let request = json!({"sandbox_id": "uncertain", "image": "alpine:3.22", "cpu_millicores": 200, "memory_bytes": 33554432});
    assert!(
        !f.run(&["place", "--json"], Some(request.clone()))
            .status
            .success()
    );
    fs::remove_file(f.root.join("uncertain")).unwrap();
    fs::remove_file(f.root.join("fail-cleanup")).unwrap();
    assert!(!f.run(&["place", "--json"], Some(request)).status.success());
    assert_eq!(
        f.inventory()["members"][0]["sandboxes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let handle = f.inventory()["members"][0]["sandboxes"][0]["handle"]
        .as_str()
        .unwrap()
        .to_string();
    f.ok(&["release", &handle], None);
    f.stop(Signal::TERM);
}

#[test]
fn sigint_and_sigterm_cancel_stalled_docker_transports() {
    for signal in [Signal::INT, Signal::TERM] {
        let mut f = Fixture::new(false);
        f.start();
        f.write("stall", "");
        let mut client = f
            .command()
            .args(["--socket", "hub.sock", "inventory", "--json"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while !f.root.join("transport").exists() {
            assert!(Instant::now() < until, "transport readiness watchdog");
            std::thread::sleep(Duration::from_millis(10));
        }
        let pids = fs::read_to_string(f.root.join("transport")).unwrap();
        f.stop(signal);
        assert!(!client.wait().unwrap().success());
        for pid in pids.split_whitespace() {
            let until = Instant::now() + Duration::from_secs(5);
            while let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
                if stat.rsplit_once(')').unwrap().1.starts_with(" Z") {
                    break;
                }
                assert!(Instant::now() < until, "transport survived hub shutdown");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[test]
fn overlong_socket_paths_report_length_and_work_with_a_relative_path() {
    let mut f = Fixture::new(false);
    let parent = f.root.join("d".repeat(120));
    fs::DirBuilder::new().mode(0o700).create(&parent).unwrap();
    let socket = parent.join("hub.sock");
    let error = std::os::unix::net::UnixListener::bind(&socket).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(error.raw_os_error(), None);
    assert!(!socket.exists());

    let expected =
        "homepool: hub socket path is too long for a Unix socket; use a shorter or relative path\n";
    for args in [
        vec!["hub", "--config", "members.toml"],
        vec!["inventory", "--json"],
    ] {
        let output = f
            .command()
            .args(args)
            .arg("--socket")
            .arg(&socket)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(String::from_utf8(output.stderr).unwrap(), expected);
        assert!(!socket.exists());
    }

    // The same directory entry is usable with a short relative socket address.
    f.hub = Some(
        f.command()
            .current_dir(&parent)
            .arg("--config")
            .arg(f.root.join("members.toml"))
            .args(["--socket", "hub.sock", "hub"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(f.hub.as_mut().unwrap().try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "hub startup watchdog");
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = f
        .command()
        .current_dir(&parent)
        .args(["--socket", "hub.sock", "inventory", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = f
        .command()
        .arg("--socket")
        .arg(&socket)
        .args(["inventory", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(String::from_utf8(output.stderr).unwrap(), expected);
    assert!(socket.exists());
    f.stop(Signal::TERM);
    assert!(!socket.exists());
}

#[test]
fn private_paths_and_member_updates_fail_closed() {
    let mut f = Fixture::new(false);
    f.start();
    let original = fs::read(f.root.join("members.toml")).unwrap();
    f.write("duplicate.toml", &f.member(1, "active", 1000, 3));
    assert!(!f.run(&["add", "duplicate.toml"], None).status.success());
    f.write("invalid.toml", "version = 2\nmembers = []");
    assert!(!f.run(&["add", "invalid.toml"], None).status.success());
    assert_eq!(fs::read(f.root.join("members.toml")).unwrap(), original);
    assert!(
        !f.command()
            .args(["hub", "--config", "members.toml", "--socket", "other.sock"])
            .output()
            .unwrap()
            .status
            .success()
    );
    fs::set_permissions(f.root.join("hub.sock"), fs::Permissions::from_mode(0o660)).unwrap();
    assert!(!f.run(&["inventory", "--json"], None).status.success());
    fs::set_permissions(f.root.join("hub.sock"), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!f.run(&["place", "--json"], Some(json!({"sandbox_id": "unaligned", "image": "alpine:3.22", "cpu_millicores": 201, "memory_bytes": 33554432}))).status.success());
    f.stop(Signal::TERM);
    f.write("hub.sock", "stale-path");
    let output = f
        .command()
        .args(["hub", "--config", "members.toml", "--socket", "hub.sock"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "homepool: could not bind hub socket; an existing path is never removed automatically\n"
    );
    assert_eq!(
        fs::read_to_string(f.root.join("hub.sock")).unwrap(),
        "stale-path"
    );
}

fn process_sample(pid: u32) -> (u64, u64, u64) {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<_> = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .collect();
    let ticks = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let kib = |key| {
        status
            .lines()
            .find(|l| l.starts_with(key))
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u64>()
            .unwrap()
    };
    (ticks, kib("VmRSS:"), kib("VmHWM:"))
}

fn placement(identity: &str, fresh: bool, preferences: &[u64]) -> Value {
    json!({"sandbox_id": identity, "image": "alpine:3.22", "cpu_millicores": 200, "memory_bytes": 33554432, "command": ["sleep", "300"], "fresh": fresh, "preferences": preferences})
}

impl Fixture {
    fn members(&self, count: u64, slots: u32) {
        let mut config = String::from("version = 1\n");
        for member in 1..=count {
            config.push_str(
                self.member(member, "active", 1000, slots)
                    .strip_prefix("version = 1\n")
                    .unwrap(),
            );
        }
        self.write("members.toml", &config);
    }
    fn pending_client(&self, args: &[&str], input: Option<&Value>) -> Child {
        let mut child = self
            .command()
            .args(["--socket", "hub.sock"])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(input) = input {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(serde_json::to_string(input).unwrap().as_bytes())
                .unwrap();
        }
        child.stdin.take();
        child
    }
    fn wait_file(&self, name: &str) {
        let until = Instant::now() + Duration::from_secs(10);
        while !self.root.join(name).exists() {
            assert!(Instant::now() < until, "engine readiness watchdog");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn engine(&self) -> Value {
        serde_json::from_slice(&fs::read(self.root.join("engine.json")).unwrap()).unwrap()
    }
    fn save_engine(&self, engine: &Value) {
        self.write("engine.json", &serde_json::to_string(engine).unwrap());
    }
}

#[test]
fn two_member_retries_are_strict_and_fresh_work_survives_an_outage() {
    for failure in ["start", "inspect", "restart"] {
        let mut f = Fixture::new(false);
        f.members(2, 3);
        f.start();
        let submitted = placement("submitted-once", false, &[1]);
        if failure != "restart" {
            f.write(
                if failure == "start" {
                    "uncertain-unavailable-1"
                } else {
                    "unconfirmed-1"
                },
                "",
            );
            assert!(
                !f.run(&["place", "--json"], Some(submitted.clone()))
                    .status
                    .success()
            );
            let view = f.inventory();
            let observed = &view["members"][0]["sandboxes"];
            assert_eq!(
                observed.as_array().unwrap().len(),
                1,
                "known failed-start handle must survive failed inventory"
            );
            assert_eq!(observed[0]["sandbox_id"], "submitted-once");
            let mut false_promise = submitted.clone();
            false_promise["fresh"] = json!(true);
            assert!(
                !f.run(&["place", "--json"], Some(false_promise))
                    .status
                    .success(),
                "observed identities are rejected even with fresh"
            );
        } else {
            f.ok(&["place", "--json"], Some(submitted.clone()));
            f.stop(Signal::TERM);
            f.write("unavailable-1", "");
            f.start(); // No old in-memory identities survive this new process.
            assert!(
                f.inventory()["members"][0]["sandboxes"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(
            !f.run(&["place", "--json"], Some(submitted))
                .status
                .success()
        );
        assert!(
            !f.run(
                &["place", "--json"],
                Some(placement("strict-new", false, &[]))
            )
            .status
            .success()
        );
        let fresh = f.ok(
            &["place", "--json"],
            Some(placement("never-submitted", true, &[])),
        );
        assert_eq!(fresh["member"], 2);
        assert_eq!(
            f.engine()["items"]
                .as_object()
                .unwrap()
                .values()
                .filter(|s| s["labels"]["homepool.sandbox"] == "submitted-once")
                .count(),
            1
        );
        f.release(&fresh);
        f.stop(Signal::TERM);
    }
}

#[test]
fn restart_settles_delayed_engine_completion_and_keeps_over_budget_inventory_readable() {
    let mut f = Fixture::new(false);
    f.operation_ms = 2500;
    f.members(1, 1);
    f.start();
    let old_pid = f.hub.as_ref().unwrap().id();
    f.write("delay-create", "");
    let client = f.pending_client(
        &["place", "--json"],
        Some(&placement("delayed", false, &[])),
    );
    f.wait_file("pending-create");
    f.stop(Signal::TERM);
    assert!(!client.wait_with_output().unwrap().status.success());
    f.spawn();
    assert_ne!(f.hub.as_ref().unwrap().id(), old_pid);
    f.wait_file("hub.sock");
    let early = f.inventory();
    assert_eq!(early["settling"], true);
    assert!(
        early["members"][0]["sandboxes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for fresh in [false, true] {
        let out = f.run(&["place", "--json"], Some(placement("delayed", fresh, &[])));
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("settling"));
    }
    // Draining is safe during settling, but an empty listing cannot justify
    // deleting the member and its endpoint before the pending create lands.
    f.ok(&["drain", "1"], None);
    let declared = fs::read(f.root.join("members.toml")).unwrap();
    let removed = f.run(&["remove", "1"], None);
    assert!(!removed.status.success());
    assert!(String::from_utf8_lossy(&removed.stderr).contains("settling"));
    assert_eq!(fs::read(f.root.join("members.toml")).unwrap(), declared);
    assert_eq!(f.inventory()["members"][0]["member"], 1);
    // Model completion by the engine after the old CLI has been killed. Only
    // the engine record, never an old hub map, crosses the process boundary.
    let pending: Value =
        serde_json::from_slice(&fs::read(f.root.join("pending-create")).unwrap()).unwrap();
    let id = pending["id"].as_str().unwrap().to_owned();
    let mut engine = f.engine();
    engine["items"][&id] = pending.clone();
    f.save_engine(&engine);
    fs::remove_file(f.root.join("delay-create")).unwrap();
    f.wait_settled();
    let view = f.inventory();
    assert_eq!(view["members"][0]["inventory_complete"], true);
    assert_eq!(view["members"][0]["state"], "draining");
    assert_eq!(view["members"][0]["sandboxes"][0]["state"], "created");
    assert_eq!(
        view["members"][0]["sandboxes"][0]["handle"],
        format!("1:{id}")
    );
    assert_eq!(view["members"][0]["free"]["sandbox_slots"], 0);
    assert_eq!(view["members"][0]["sandboxes"].as_array().unwrap().len(), 1);
    assert!(
        !f.run(&["place", "--json"], Some(placement("delayed", false, &[])))
            .status
            .success()
    );
    // Existing/legacy over-budget work must remain readable, including when
    // two observations compete for a one-slot declared budget.
    let legacy_id = "f".repeat(64);
    let mut legacy = pending;
    legacy["id"] = json!(legacy_id);
    legacy["name"] = json!("legacy-duplicate");
    engine["items"][&legacy_id] = legacy;
    f.save_engine(&engine);
    let over = f.inventory();
    assert_eq!(over["members"][0]["inventory_complete"], false);
    assert_eq!(over["members"][0]["free"]["sandbox_slots"], 0);
    assert_eq!(over["members"][0]["sandboxes"].as_array().unwrap().len(), 2);
    f.ok(&["release", &format!("1:{id}")], None);
    assert_eq!(f.inventory()["members"][0]["inventory_complete"], true);
    f.ok(&["release", &format!("1:{legacy_id}")], None);
    f.ok(&["remove", "1"], None);
    assert!(f.inventory()["members"].as_array().unwrap().is_empty());
    f.stop(Signal::TERM);
}

#[test]
fn several_stalled_members_leave_inventory_release_and_drain_usable_and_expired_queue_work_never_starts()
 {
    let mut f = Fixture::new(false);
    f.operation_ms = 3000;
    f.request_ms = 4000; // Two full member waits would exceed this request.
    f.members(3, 3);
    f.start();
    let first = f.ok(
        &["place", "--json"],
        Some(placement("existing", false, &[3])),
    );
    f.write("stall-1", "");
    f.write("stall-2", "");
    let started = Instant::now();
    let view = f.inventory();
    assert!(started.elapsed() < Duration::from_millis(f.request_ms));
    assert_eq!(view["members"][0]["inventory_complete"], false);
    assert_eq!(view["members"][1]["inventory_complete"], false);
    assert_eq!(view["members"][2]["inventory_complete"], true);
    assert_eq!(
        view["members"][2]["sandboxes"][0]["handle"],
        first["handle"]
    );
    let placed = f.ok(
        &["place", "--json"],
        Some(placement("fresh-during-outage", true, &[3])),
    );
    let started = Instant::now();
    f.release(&placed);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "release must not probe unrelated stalled members"
    );
    fs::remove_file(f.root.join("transport")).unwrap();
    let waiting_inventory = f.pending_client(&["inventory", "--json"], None);
    f.wait_file("transport");
    // This client expires while its frame is queued behind the inventory.
    // The server must use the original deadline, not grant a new window.
    let mut queued = f
        .command()
        .env("HOMEPOOL_REQUEST_TIMEOUT_MS", "300")
        .args(["--socket", "hub.sock", "place", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    queued
        .stdin
        .take()
        .unwrap()
        .write_all(
            serde_json::to_string(&placement("expired-in-queue", true, &[3]))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
    let expired = queued.wait_with_output().unwrap();
    assert!(!expired.status.success());
    assert!(String::from_utf8_lossy(&expired.stderr).contains("deadline"));
    assert!(
        waiting_inventory
            .wait_with_output()
            .unwrap()
            .status
            .success()
    );
    // A subsequent response proves the queued frame has been processed.
    let reconciled = f.inventory();
    assert_eq!(
        reconciled["members"][2]["sandboxes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        !f.engine()["items"]
            .as_object()
            .unwrap()
            .values()
            .any(|s| s["labels"]["homepool.sandbox"] == "expired-in-queue")
    );
    let started = Instant::now();
    f.ok(&["drain", "3"], None);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "drain must not probe unrelated stalled members"
    );
    f.release(&first);
    f.stop(Signal::TERM);
}

fn late_create_stays_created(restart: bool) {
    let mut f = Fixture::new(false);
    f.operation_ms = 2500;
    f.members(2, 2);
    f.start();
    let old_pid = f.hub.as_ref().unwrap().id();
    f.write("delay-create", "");
    let mut command = f.command();
    if !restart {
        // The request expires well before the adapter's configured limit,
        // while this same hub process remains alive and can serve a retry.
        command.env("HOMEPOOL_REQUEST_TIMEOUT_MS", "1000");
    }
    let mut client = command
        .args(["--socket", "hub.sock", "place", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    client
        .stdin
        .take()
        .unwrap()
        .write_all(
            serde_json::to_string(&placement("late-create", false, &[1]))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
    f.wait_file("pending-create");
    if restart {
        f.stop(Signal::TERM);
    }
    let failed = client.wait_with_output().unwrap();
    assert!(!failed.status.success());
    if restart {
        f.start(); // Wait through the entire new process's settle period.
        assert_ne!(f.hub.as_ref().unwrap().id(), old_pid);
    } else {
        assert_eq!(f.hub.as_ref().unwrap().id(), old_pid);
        assert!(
            String::from_utf8_lossy(&failed.stderr).contains("Docker create deadline exceeded")
        );
    }
    let empty = f.inventory();
    assert_eq!(empty["settling"], false);
    assert!(
        empty["members"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["inventory_complete"] == true
                && m["sandboxes"].as_array().unwrap().is_empty())
    );
    // Permit a strict retry on B before the original create on A completes.
    // The pending RPC belongs to the engine; its dead CLI cannot issue start.
    fs::remove_file(f.root.join("delay-create")).unwrap();
    let running = f.ok(
        &["place", "--json"],
        Some(placement("late-create", false, &[2])),
    );
    assert_eq!(running["member"], 2);
    let pending: Value =
        serde_json::from_slice(&fs::read(f.root.join("pending-create")).unwrap()).unwrap();
    let late_id = pending["id"].as_str().unwrap();
    let mut engine = f.engine();
    engine["items"][late_id] = pending.clone();
    f.save_engine(&engine);
    let view = f.inventory();
    let observations: Vec<_> = view["members"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["sandboxes"].as_array().unwrap())
        .collect();
    assert_eq!(observations.len(), 2);
    assert!(
        observations
            .iter()
            .all(|s| s["sandbox_id"] == "late-create")
    );
    assert_eq!(
        observations
            .iter()
            .filter(|s| s["state"] == "running")
            .count(),
        1
    );
    assert_eq!(
        observations
            .iter()
            .filter(|s| s["state"] == "created")
            .count(),
        1
    );
    let late_handle = format!("1:{late_id}");
    assert_eq!(view["members"][0]["sandboxes"][0]["handle"], late_handle);
    assert_eq!(view["members"][0]["sandboxes"][0]["state"], "created");
    assert_eq!(view["members"][0]["free"]["cpu_millicores"], 800);
    assert_eq!(
        view["members"][0]["free"]["memory_bytes"],
        268435456 - 33554432
    );
    assert_eq!(view["members"][0]["free"]["sandbox_slots"], 1);
    f.release(&running);
    let only_late = f.inventory();
    assert!(
        only_late["members"][1]["sandboxes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(only_late["members"][0]["sandboxes"][0]["state"], "created");
    assert!(
        !f.run(
            &["place", "--json"],
            Some(placement("late-create", false, &[1]))
        )
        .status
        .success()
    );
    assert_eq!(
        f.engine()["items"][late_id]["state"],
        "created",
        "the late record is never started"
    );
    f.ok(&["release", &late_handle], None);
    let released = f.inventory();
    assert!(
        released["members"][0]["sandboxes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(released["members"][0]["free"]["cpu_millicores"], 1000);
    assert_eq!(released["members"][0]["free"]["memory_bytes"], 268435456);
    assert_eq!(released["members"][0]["free"]["sandbox_slots"], 2);
    assert!(
        !f.engine()["items"]
            .as_object()
            .unwrap()
            .contains_key(late_id)
    );
    assert!(
        released["members"][1]["sandboxes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    f.stop(Signal::TERM);
}

#[test]
fn late_create_after_a_short_request_never_starts_and_holds_capacity() {
    late_create_stays_created(false);
}

#[test]
fn late_create_after_sigterm_and_full_settle_never_starts_and_holds_capacity() {
    late_create_stays_created(true);
}

fn profile_placement(name: &str, profile: &str) -> Value {
    json!({"sandbox_id":name,"image":"alpine:3.22","cpu_millicores":100,"memory_bytes":33554432,"profile":profile,"command":["sh","-c","trap 'exit 0' TERM; while :; do sleep 1 & wait $!; done"]})
}

#[test]
fn invalid_profiles_fail_on_read_and_add_without_docker_calls() {
    let mut f = Fixture::new(false);
    f.start();
    let cases = [
        ("bad/name", ""),
        ("", ""),
        ("bad,name", ""),
        ("bad\nname", ""),
        ("-option", ""),
        ("runner", "pids_limit = 0"),
        ("runner", "shm_bytes = 0"),
        ("runner", "memory_swap_bytes = 0"),
        ("runner", "pids_limit = -1"),
        ("runner", "privileged = 1"),
        ("runner", "flags = ['--privileged']"),
        (
            "runner",
            "binds = [{source = 'relative', target = '/input'}]",
        ),
        (
            "runner",
            "binds = [{source = '/srv/input', target = 'relative'}]",
        ),
        (
            "runner",
            "binds = [{source = '/srv/in,put', target = '/input'}]",
        ),
        (
            "runner",
            "binds = [{source = '/srv/input', target = '/in,put'}]",
        ),
        (
            "runner",
            "binds = [{source = '/srv/in\"put', target = '/input'}]",
        ),
        (
            "runner",
            "binds = [{source = \"/srv/in\\nput\", target = '/input'}]",
        ),
        (
            "runner",
            "binds = [{source = '/srv/input', target = \"/in\\u0000put\"}]",
        ),
        (
            "runner",
            "volumes = [{name = '../cache', target = '/cache'}]",
        ),
        (
            "runner",
            "volumes = [{name = 'cache', target = 'relative'}]",
        ),
        (
            "runner",
            "volumes = [{name = 'cache.data', target = '/cache'}]",
        ),
        ("runner", "volumes = [{name = 'cache', target = '/ca,che'}]"),
        (
            "runner",
            "volumes = [{name = 'cache', target = \"/ca\\tche\"}]",
        ),
        ("runner", "tmpfs = [{target = 'relative', options = 'rw'}]"),
        (
            "runner",
            "tmpfs = [{target = '/scratch:ro', options = 'rw'}]",
        ),
        ("runner", "tmpfs = [{target = '/scr,atch', options = 'rw'}]"),
        (
            "runner",
            "tmpfs = [{target = \"/scr\\natch\", options = 'rw'}]",
        ),
        (
            "runner",
            "tmpfs = [{target = '/scratch', options = 'rw:ro'}]",
        ),
        (
            "runner",
            "tmpfs = [{target = '/scratch', options = 'rw;exec'}]",
        ),
        (
            "runner",
            "tmpfs = [{target = '/scratch', options = \"rw\\nexec\"}]",
        ),
        (
            "runner",
            "tmpfs = [{target = '/scratch', options = 'rw size=1m'}]",
        ),
        (
            "runner",
            "tmpfs = [{target = '/scratch', options = 'rw', unknown = true}]",
        ),
    ];
    let oversized = format!(
        "volumes = [{{name = '{}', target = '/cache'}}]",
        "v".repeat(97)
    );
    // add must not probe an engine, even for rejected member definitions.
    f.write("stall", "");
    for (name, fields) in cases.into_iter().chain([("runner", oversized.as_str())]) {
        let document = format!(
            "{}\n[members.capabilities.compute.profiles.{}]\n{fields}\n",
            f.member(2, "active", 1000, 2),
            serde_json::to_string(name).unwrap()
        );
        f.write("addition.toml", &document);
        let status = f.run(&["--config", "addition.toml", "status", "--json"], None);
        assert!(
            !status.status.success(),
            "accepted profile {name}: {fields}"
        );
        assert!(status.stdout.is_empty());
        let add = f.run(&["add", "addition.toml"], None);
        assert!(
            !add.status.success(),
            "added invalid profile {name}: {fields}"
        );
        assert!(String::from_utf8_lossy(&add.stderr).contains("invalid member addition"));
        assert!(
            !f.root.join("transport").exists(),
            "invalid profile contacted Docker"
        );
    }
}

#[test]
fn profiles_arrive_through_add_and_survive_drain_and_restart() {
    let mut f = Fixture::new(false);
    f.start();
    let addition = format!(
        "{}\n[members.capabilities.compute.profiles.runner]\npids_limit = 100\n",
        f.member(2, "active", 1000, 1)
    );
    f.write("addition.toml", &addition);
    f.ok(&["add", "addition.toml"], None);
    let missing = f.run(
        &["place", "--json"],
        Some(profile_placement("missing", "absent")),
    );
    assert!(!missing.status.success());
    let placed = f.ok(
        &["place", "--json"],
        Some(profile_placement("profile-one", "runner")),
    );
    assert_eq!(placed["member"], 2);
    let full = f.run(
        &["place", "--json"],
        Some(profile_placement("profile-two", "runner")),
    );
    assert!(!full.status.success());
    f.ok(&["drain", "2"], None);
    f.stop(Signal::TERM);
    f.start();
    assert_eq!(
        f.inventory()["members"][1]["sandboxes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    f.release(&placed);
    let drained = f.run(
        &["place", "--json"],
        Some(profile_placement("profile-three", "runner")),
    );
    assert!(!drained.status.success());
    let plain = f.place("plain", 100, 33554432);
    assert_eq!(plain["member"], 1);
    f.release(&plain);
    f.ok(&["remove", "2"], None);
    assert!(
        !fs::read_to_string(f.root.join("members.toml"))
            .unwrap()
            .contains("runner")
    );
}

#[test]
fn selected_profile_swap_below_memory_is_rejected_before_create() {
    let mut f = Fixture::new(false);
    f.write(
        "members.toml",
        &format!(
            "{}\n[members.capabilities.compute.profiles.runner]\nmemory_swap_bytes = 16777216\n",
            f.member(1, "active", 1000, 2)
        ),
    );
    let second = format!(
        "{}\n[members.capabilities.compute.profiles.runner]\nmemory_swap_bytes = 67108864\n",
        f.member(2, "active", 1000, 2)
    );
    let first = fs::read_to_string(f.root.join("members.toml")).unwrap();
    f.write(
        "members.toml",
        &format!("{first}{}", second.strip_prefix("version = 1\n").unwrap()),
    );
    f.start();
    let failed = f.run(
        &["place", "--json"],
        Some(profile_placement("swap-too-small", "runner")),
    );
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr)
            .contains("profile memory swap must be at least requested memory")
    );
    assert!(!f.root.join("engine.json").exists());
    assert_eq!(f.inventory()["members"][0]["free"]["sandbox_slots"], 2);
    let mut request = profile_placement("swap-fits-selected-member", "runner");
    request["preferences"] = json!([2]);
    let placed = f.ok(&["place", "--json"], Some(request));
    assert_eq!(placed["member"], 2);
    f.release(&placed);
}

#[test]
fn placement_accepts_only_a_profile_name_and_no_direct_launch_settings() {
    let f = Fixture::new(false);
    for (key, value) in [
        ("profile", json!({"privileged":true})),
        ("privileged", json!(true)),
        ("binds", json!([])),
        ("volumes", json!([])),
        ("tmpfs", json!([])),
        ("flags", json!(["--privileged"])),
    ] {
        let mut request = profile_placement("invalid", "runner");
        request[key] = value;
        let failed = f.run(&["place", "--json"], Some(request));
        assert!(!failed.status.success());
        assert!(String::from_utf8_lossy(&failed.stderr).contains("invalid placement JSON"));
    }
}

fn profile_docker(args: &[&str]) -> Output {
    Command::new("docker")
        .arg("--host")
        .arg(
            std::env::var("HOMEPOOL_TEST_DOCKER_SOCKET")
                .map(|socket| format!("unix://{socket}"))
                .unwrap_or_else(|_| "unix:///var/run/docker.sock".into()),
        )
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_TLS")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

struct ProfileVolume {
    name: String,
    container_name: String,
}
impl Drop for ProfileVolume {
    fn drop(&mut self) {
        // Remove this test's exact container first, including after assertions.
        let _ = profile_docker(&["container", "rm", "--force", &self.container_name]);
        let _ = profile_docker(&["volume", "rm", &self.name]);
    }
}

#[test]
#[ignore = "requires a local Docker engine and alpine:3.22"]
fn real_profile_placement_inspects_every_field_and_release_keeps_volume() {
    let mut f = Fixture::new(true);
    let identity = f.root.file_name().unwrap().to_str().unwrap().to_string();
    let input = f.root.join("input");
    fs::create_dir(&input).unwrap();
    let profile = format!(
        r#"
[members.capabilities.compute.profiles.runner]
privileged = true
pids_limit = 100
shm_bytes = 16777216
memory_swap_bytes = 67108864
tmpfs = [{{target = "/scratch", options = "rw,size=8m,mode=1770"}}]
volumes = [{{name = "cache", target = "/cache", read_only = true}}]
binds = [{{source = {}, target = "/input", read_only = true}}]
"#,
        serde_json::to_string(input.to_str().unwrap()).unwrap()
    );
    f.write(
        "members.toml",
        &format!("{}{profile}", f.member(3, "active", 1000, 2)),
    );
    f.start();
    // Volumes intentionally outlive release; only the test owns their cleanup.
    let volume = ProfileVolume {
        name: format!("homepool-3-{identity}.cache"),
        container_name: format!("homepool-3-{identity}"),
    };
    let placed = f.ok(
        &["place", "--json"],
        Some(profile_placement(&identity, "runner")),
    );
    assert_eq!(placed["member"], 3);
    let container = placed["handle"]
        .as_str()
        .unwrap()
        .split_once(':')
        .unwrap()
        .1;
    f.created.borrow_mut().push((3, container.into()));
    let inspected = profile_docker(&["container", "inspect", container]);
    assert!(inspected.status.success());
    let details: Value = serde_json::from_slice(&inspected.stdout).unwrap();
    let host = &details[0]["HostConfig"];
    assert_eq!(host["Privileged"], true);
    assert_eq!(host["PidsLimit"], 100);
    assert_eq!(host["ShmSize"], 16777216);
    assert_eq!(host["MemorySwap"], 67108864);
    assert_eq!(host["Memory"], 33554432);
    assert_eq!(host["NanoCpus"], 100000000);
    assert_eq!(host["Tmpfs"]["/scratch"], "rw,size=8m,mode=1770");
    assert_eq!(host["RestartPolicy"]["Name"], "no");
    let mounts = details[0]["Mounts"].as_array().unwrap();
    let cache = mounts
        .iter()
        .find(|m| m["Destination"] == "/cache")
        .unwrap();
    assert_eq!(cache["Type"], "volume");
    assert_eq!(cache["Name"], volume.name);
    assert_eq!(cache["RW"], false);
    let bind = mounts
        .iter()
        .find(|m| m["Destination"] == "/input")
        .unwrap();
    assert_eq!(bind["Type"], "bind");
    assert_eq!(bind["Source"], input.to_str().unwrap());
    assert_eq!(bind["RW"], false);
    // A fresh hub accounts for this ordinary bounded allocation unchanged.
    f.stop(Signal::TERM);
    f.start();
    assert_eq!(
        f.inventory()["members"][0]["sandboxes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    f.release(&placed);
    assert!(
        profile_docker(&["volume", "inspect", &volume.name])
            .status
            .success()
    );
}

#[test]
#[ignore = "requires a local Docker engine and alpine:3.22"]
fn real_profile_volumes_keep_ambiguous_boundaries_and_retained_data_separate() {
    let mut f = Fixture::new(true);
    let base = f.root.file_name().unwrap().to_str().unwrap();
    let identities = [format!("{base}-job-a"), format!("{base}-job")];
    f.write(
        "members.toml",
        &format!(
            r#"{}
[members.capabilities.compute.profiles.runner]
volumes = [{{name = "cache", target = "/cache"}}, {{name = "a-cache", target = "/a-cache"}}]
"#,
            f.member(4, "active", 1000, 2)
        ),
    );
    let volumes: Vec<_> = identities
        .iter()
        .flat_map(|identity| {
            ["cache", "a-cache"].map(|suffix| ProfileVolume {
                name: format!("homepool-4-{identity}.{suffix}"),
                container_name: format!("homepool-4-{identity}"),
            })
        })
        .collect();
    f.start();
    for retained in [false, true] {
        let mut placements = Vec::new();
        let mut names = std::collections::BTreeSet::new();
        for (index, identity) in identities.iter().enumerate() {
            let placed = f.ok(
                &["place", "--json"],
                Some(profile_placement(identity, "runner")),
            );
            let container = placed["handle"]
                .as_str()
                .unwrap()
                .split_once(':')
                .unwrap()
                .1;
            f.created.borrow_mut().push((4, container.into()));
            let inspected = profile_docker(&["container", "inspect", container]);
            assert!(inspected.status.success());
            let details: Value = serde_json::from_slice(&inspected.stdout).unwrap();
            let mounts = details[0]["Mounts"].as_array().unwrap();
            for suffix in ["cache", "a-cache"] {
                let target = format!("/{suffix}");
                let mount = mounts
                    .iter()
                    .find(|mount| mount["Destination"] == target)
                    .unwrap();
                assert_eq!(mount["Name"], format!("homepool-4-{identity}.{suffix}"));
                assert!(names.insert(mount["Name"].as_str().unwrap().to_string()));
                let file = format!("{target}/value");
                let expected = format!("{index}-{suffix}");
                if !retained {
                    assert!(
                        profile_docker(&[
                            "container",
                            "exec",
                            container,
                            "sh",
                            "-c",
                            "printf '%s' \"$1\" > \"$2\"",
                            "sh",
                            &expected,
                            &file
                        ])
                        .status
                        .success()
                    );
                } else {
                    let read = profile_docker(&["container", "exec", container, "cat", &file]);
                    assert!(read.status.success());
                    assert_eq!(read.stdout, expected.as_bytes());
                }
            }
            placements.push(placed);
        }
        assert_eq!(names.len(), 4);
        for placed in &placements {
            f.release(placed);
        }
        for volume in &volumes {
            assert!(
                profile_docker(&["volume", "inspect", &volume.name])
                    .status
                    .success()
            );
        }
    }
}

fn job_results(real: bool) {
    let mut f = Fixture::new(real);
    f.start();
    let placed = f.ok(
        &["place", "--json"],
        Some(json!({
            "sandbox_id": "job-results", "image": "alpine:3.22",
            "cpu_millicores": 100, "memory_bytes": 33554432,
            "command": ["sh", "-c", "printf 'job output\\n'; printf 'job error\\n' >&2; exit 3"]
        })),
    );
    let handle = placed["handle"].as_str().unwrap();
    let (_, id) = handle.split_once(':').unwrap();
    if real {
        f.created.borrow_mut().push((1, id.into()));
    } else {
        let mut engine = f.engine();
        engine["items"][id]["state"] = json!("exited");
        engine["items"][id]["exit_code"] = json!(3);
        engine["items"][id]["stdout"] = json!(b"job output\n");
        engine["items"][id]["stderr"] = json!(b"job error\n");
        f.save_engine(&engine);
    }
    let until = Instant::now() + Duration::from_secs(10);
    let before = loop {
        let inventory = f.inventory();
        if inventory["members"][0]["sandboxes"][0]["state"] == "exited" {
            break inventory;
        }
        assert!(Instant::now() < until, "job exit watchdog");
    };
    assert_eq!(before["members"][0]["sandboxes"][0]["exit_code"], 3);
    assert_eq!(before["members"][0]["free"]["cpu_millicores"], 900);
    assert_eq!(before["members"][0]["free"]["sandbox_slots"], 2);
    let expected = json!({"stdout": "job output\n", "stderr": "job error\n", "cut": false});
    assert_eq!(f.ok(&["logs", handle], None), expected);
    f.stop(Signal::TERM);
    f.spawn();
    // Read-only logs are usable even during the new process's settle window.
    f.wait_file("hub.sock");
    assert_eq!(f.ok(&["logs", handle], None), expected);
    f.wait_settled();
    assert_eq!(f.inventory(), before);
    assert_eq!(f.ok(&["logs", handle], None), expected);
    f.release(&placed);
}

#[test]
fn job_results_and_bounded_logs() {
    // Run the new process-heavy scenarios sequentially so they add only one
    // concurrent fixture to the existing short-request deadline regressions.
    job_results(false);
    logs_bound_lines_and_bytes_and_replace_non_utf8();
    logs_refuse_unknown_unmanaged_and_unreachable_without_changing_inventory();
}

#[test]
#[ignore = "requires a disposable Docker engine and alpine:3.22"]
fn engine_job_exit_and_logs_survive_process_restart() {
    job_results(true);
}

fn logs_bound_lines_and_bytes_and_replace_non_utf8() {
    let mut f = Fixture::new(false);
    f.start();
    let placed = f.place("bounded-logs", 100, 33554432);
    let handle = placed["handle"].as_str().unwrap();
    let (_, id) = handle.split_once(':').unwrap();
    let before = f.inventory();
    assert!(
        before["members"][0]["sandboxes"][0]
            .get("exit_code")
            .is_none()
    );
    let mut engine = f.engine();
    engine["items"][id]["stdout"] = json!(b"old\nlast\n");
    engine["items"][id]["stderr"] = json!([255, 10]);
    f.save_engine(&engine);
    assert_eq!(
        f.ok(&["logs", handle, "--tail", "1"], None),
        json!({"stdout":"last\n", "stderr":"\u{fffd}\n", "cut":true})
    );
    assert_eq!(
        f.ok(&["logs", handle, "--tail", "0"], None),
        json!({"stdout":"", "stderr":"", "cut":true})
    );
    engine["items"][id]["stdout"] = json!(
        (0..110)
            .flat_map(|_| b"line\n")
            .copied()
            .collect::<Vec<_>>()
    );
    f.save_engine(&engine);
    let default = f.ok(&["logs", handle], None);
    assert_eq!(default["stdout"], "line\n".repeat(100));
    assert_eq!(default["cut"], true);
    // Worst JSON escaping on both full pipes must still fit the wire envelope.
    engine["items"][id]["stdout"] = json!(vec![0u8; 200_000]);
    engine["items"][id]["stderr"] = json!(vec![1u8; 200_000]);
    f.save_engine(&engine);
    let output = f.run(&["logs", handle, "--tail", "1000"], None);
    assert!(output.status.success());
    assert!(output.stdout.len() + 128 < 1024 * 1024);
    let logs: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(logs["cut"], true);
    assert!(!logs["stdout"].as_str().unwrap().is_empty());
    assert!(!logs["stderr"].as_str().unwrap().is_empty());
    assert_eq!(f.inventory(), before);
}

fn logs_refuse_unknown_unmanaged_and_unreachable_without_changing_inventory() {
    let mut f = Fixture::new(false);
    f.start();
    let placed = f.place("private-logs", 100, 33554432);
    let handle = placed["handle"].as_str().unwrap();
    let (_, id) = handle.split_once(':').unwrap();
    let before = f.inventory();
    let mut engine = f.engine();
    engine["items"][id]["stdout"] = json!(b"private output");
    engine["items"][id]["stderr"] = json!(b"private error");
    f.save_engine(&engine);
    for unknown in [
        format!("1:{}", "f".repeat(64)),
        format!("99:{id}"),
        "invalid".into(),
    ] {
        let output = f.run(&["logs", &unknown], None);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!f.root.join("logs-read").exists());
    }
    for label in ["homepool.managed", "homepool.member", "homepool.sandbox"] {
        let mut unowned = engine.clone();
        unowned["items"][id]["labels"]
            .as_object_mut()
            .unwrap()
            .remove(label);
        f.save_engine(&unowned);
        let output = f.run(&["logs", handle], None);
        assert_eq!(output.stderr, b"homepool: could not read sandbox logs\n");
        assert!(output.stdout.is_empty());
        assert!(!f.root.join("logs-read").exists());
    }
    f.save_engine(&engine);
    for flag in ["unavailable-1", "fail-logs", "stall-logs"] {
        f.write(flag, "");
        let output = f.run(&["logs", handle], None);
        assert_eq!(output.stderr, b"homepool: could not read sandbox logs\n");
        assert!(output.stdout.is_empty());
        fs::remove_file(f.root.join(flag)).unwrap();
        assert_eq!(f.inventory(), before);
    }
    // No unrelated probes: a healthy exact handle still works during an outage.
    f.write("addition.toml", &f.member(2, "active", 1000, 3));
    f.ok(&["add", "addition.toml"], None);
    f.write("unavailable-2", "");
    assert_eq!(f.ok(&["logs", handle], None)["stdout"], "private output");
    let out_of_range = f.run(&["logs", handle, "--tail", "1001"], None);
    assert_eq!(
        out_of_range.stderr,
        b"homepool: logs tail exceeds maximum\n"
    );
}

struct HttpApi {
    child: Option<Child>,
    address: std::net::SocketAddr,
}
const TEST_API_TOKEN: &str = "example-test-token-do-not-use";
impl HttpApi {
    fn start(f: &Fixture, socket: &str) -> Self {
        Self::configured(f, socket, false)
    }
    fn configured(f: &Fixture, socket: &str, operations: bool) -> Self {
        let reservation = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = reservation.local_addr().unwrap();
        drop(reservation);
        f.write("token", &format!("{TEST_API_TOKEN}\n"));
        let mut command = f.command();
        if operations {
            command.args(["--operations-file", "operations.toml"]);
        }
        let child = command
            .args([
                "api",
                "--listen",
                &address.to_string(),
                "--token-file",
                "token",
                "--socket",
                socket,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut api = Self {
            child: Some(child),
            address,
        };
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                api.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                "API exited at startup"
            );
            if std::net::TcpStream::connect(address).is_ok() {
                break;
            }
            assert!(Instant::now() < end, "API startup watchdog");
            std::thread::sleep(Duration::from_millis(10));
        }
        api
    }
    fn stream(&self) -> std::net::TcpStream {
        let stream = std::net::TcpStream::connect(self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(8)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
    }
    fn raw(&self, request: &str) -> (u16, String) {
        use std::io::Read;
        let mut stream = self.stream();
        stream.write_all(request.as_bytes()).unwrap();
        let mut output = String::new();
        // A server can close with unread over-limit bytes, causing a reset after
        // the response. Retain the response already read in that case.
        if let Err(error) = stream.read_to_string(&mut output) {
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        }
        assert!(!output.contains(TEST_API_TOKEN), "token in HTTP output");
        assert!(
            !output.contains(TEST_OPERATION_SECRET),
            "environment value in HTTP output"
        );
        let status = output.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, output.split_once("\r\n\r\n").unwrap().1.into())
    }
    fn post(&self, body: &str) -> (u16, String) {
        self.raw(&format!("POST /v1/jobs HTTP/1.1\r\nAuthorization: Bearer {TEST_API_TOKEN}\r\nContent-Length: {}\r\n\r\n{body}", body.len()))
    }
    fn job(&self, id: &str) -> (u16, Value) {
        let (status, body) = self.get(
            &format!("/v1/jobs/{id}"),
            Some(&format!("Bearer {TEST_API_TOKEN}")),
        );
        (status, serde_json::from_str(&body).unwrap())
    }
    fn get(&self, path: &str, auth: Option<&str>) -> (u16, String) {
        let header = auth
            .map(|a| format!("Authorization: {a}\r\n"))
            .unwrap_or_default();
        self.raw(&format!(
            "GET {path} HTTP/1.1\r\nHost: loopback\r\n{header}\r\n"
        ))
    }
}
impl Drop for HttpApi {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            for bytes in [&output.stdout, &output.stderr] {
                assert!(
                    !String::from_utf8_lossy(bytes).contains(TEST_API_TOKEN)
                        && !String::from_utf8_lossy(bytes).contains(TEST_OPERATION_SECRET),
                    "token in process output"
                );
            }
        }
    }
}

fn api_lists_sandbox(real: bool) {
    let mut f = Fixture::new(real);
    f.start();
    let placed = f.place("api-example", 100, 33554432);
    let api = HttpApi::start(&f, "hub.sock");
    let auth = format!("Bearer {TEST_API_TOKEN}");
    // Rotation takes effect only at the next API startup.
    f.write("token", "replacement-example-token");
    let (status, body) = api.get("/v1/sandboxes", Some(&auth));
    assert_eq!(status, 200);
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        body,
        json!({"inventory_complete":true,"sandboxes":[{"member":1,"sandbox_id":"api-example","state":"running","exit_code":null}]})
    );
    assert_eq!(api.get("/v1/health", None), (200, "{\"version\":1}".into()));
    f.release(&placed);
    f.stop(Signal::TERM);
    assert_eq!(api.get("/v1/sandboxes", Some(&auth)).0, 503);
    assert_eq!(api.get("/v1/health", None).0, 200);
    // A real socket trap proves rejected requests never open a hub connection.
    // Bind through a short directory-fd path, independent of scratch path length.
    let directory = fs::File::open(&f.root).unwrap();
    let trap = std::os::unix::net::UnixListener::bind(format!(
        "/proc/self/fd/{}/hub.sock",
        std::os::fd::AsRawFd::as_raw_fd(&directory)
    ))
    .unwrap();
    fs::set_permissions(f.root.join("hub.sock"), fs::Permissions::from_mode(0o600)).unwrap();
    trap.set_nonblocking(true).unwrap();
    for token in [
        None,
        Some("Bearer wrong"),
        Some("Basic example"),
        Some("Bearer"),
        Some("Bearer bad token"),
        Some("Bearer ="),
        Some("Bearer "),
    ] {
        assert_eq!(api.get("/v1/sandboxes", token).0, 401);
    }
    assert_eq!(
        api.raw(&format!(
            "GET /v1/sandboxes HTTP/1.1\r\nAuthorization: {auth}\r\nAuthorization: {auth}\r\n\r\n"
        ))
        .0,
        401
    );
    assert_eq!(api.get("/unknown", Some(&auth)).0, 404);
    for method in ["POST", "HEAD", "OPTIONS", "DELETE"] {
        assert_eq!(
            api.raw(&format!(
                "{method} /v1/sandboxes HTTP/1.1\r\nAuthorization: {auth}\r\n\r\n"
            ))
            .0,
            405
        );
    }
    assert_eq!(
        api.raw(&format!(
            "GET /v1/sandboxes HTTP/1.1\r\nAuthorization: {auth}\r\nContent-Length: 1\r\n\r\nx"
        ))
        .0,
        400
    );
    assert_eq!(api.get("/v1/health", None).0, 200);
    assert_eq!(
        trap.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn api_process_lists_and_authenticates() {
    api_lists_sandbox(false);
}
#[test]
#[ignore = "requires a disposable Docker engine and alpine:3.22"]
fn api_engine_lists_and_authenticates() {
    api_lists_sandbox(true);
}

#[test]
fn api_bounds_headers_and_stalled_connections_and_survives_hub_recovery() {
    use std::io::Read;
    let mut f = Fixture::new(false);
    let api = HttpApi::start(&f, "hub.sock");
    let mut stalled = api.stream();
    stalled
        .write_all(b"GET /v1/health HTTP/1.1\r\nX-Stalled: ")
        .unwrap();
    let start = Instant::now();
    assert_eq!(api.get("/v1/health", None).0, 200);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(
        api.raw(&format!(
            "GET /v1/health HTTP/1.1\r\nX-Large: {}\r\n\r\n",
            "x".repeat(9000)
        ))
        .0,
        431
    );
    assert_eq!(
        api.raw(&format!(
            "GET /v1/health HTTP/1.1\r\n{}\r\n",
            "X: x\r\n".repeat(33)
        ))
        .0,
        431
    );
    let mut response = String::new();
    stalled.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 408"));
    assert!(start.elapsed() < Duration::from_secs(7));
    let auth = format!("Bearer {TEST_API_TOKEN}");
    assert_eq!(api.get("/v1/sandboxes", Some(&auth)).0, 503);
    f.start();
    assert_eq!(api.get("/v1/sandboxes", Some(&auth)).0, 200);
}

#[test]
fn api_refuses_non_loopback_and_invalid_private_tokens_without_echoing_them() {
    let f = Fixture::new(false);
    let refuse = |address: &str| {
        let out = f
            .command()
            .args([
                "api",
                "--listen",
                address,
                "--token-file",
                "token",
                "--socket",
                "hub.sock",
            ])
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        let error = String::from_utf8(out.stderr).unwrap();
        assert!(!error.contains(TEST_API_TOKEN));
        error
    };
    assert!(refuse("192.0.2.1:0").contains("loopback"));
    assert!(refuse("[2001:db8::1]:0").contains("loopback"));
    let address = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)).to_string();
    assert!(refuse(&address).contains("token file"));
    for text in [
        String::new(),
        "\n".into(),
        "invalid token".into(),
        "x".repeat(257),
        format!("{TEST_API_TOKEN}\nextra"),
    ] {
        f.write("token", &text);
        assert!(refuse(&address).contains("token file"));
    }
    f.write("token", TEST_API_TOKEN);
    fs::set_permissions(f.root.join("token"), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(refuse(&address).contains("token file"));
    fs::remove_file(f.root.join("token")).unwrap();
    f.write("other-token", TEST_API_TOKEN);
    std::os::unix::fs::symlink("other-token", f.root.join("token")).unwrap();
    assert!(refuse(&address).contains("token file"));
    fs::remove_file(f.root.join("token")).unwrap();
    fs::create_dir(f.root.join("token")).unwrap();
    fs::set_permissions(f.root.join("token"), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(refuse(&address).contains("token file"));
}

const TEST_OPERATION_SECRET: &str = "private-environment-example";
fn approved_operations(f: &Fixture) {
    f.write(
        "operations.toml",
        &format!(
            r#"version = 1
[[operations]]
name = "finish"
image = "alpine:3.22"
command = ["sh", "-c", "sleep 1; exit 7"]
cpu_millicores = 100
memory_bytes = 33554432
[operations.environment]
PRIVATE_VALUE = "{TEST_OPERATION_SECRET}"
[[operations]]
name = "other"
image = "alpine:3.22"
command = ["true"]
cpu_millicores = 100
memory_bytes = 33554432
"#
        ),
    );
}
fn api_submits_approved_job(real: bool) {
    let mut f = Fixture::new(real);
    f.start();
    approved_operations(&f);
    let api = HttpApi::configured(&f, "hub.sock", true);
    let auth = format!("Bearer {TEST_API_TOKEN}");
    let (status, body) = api.get("/v1/operations", Some(&auth));
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!(["finish", "other"])
    );
    // Neither subsequent edits nor secrets become public catalog contents.
    f.write("operations.toml", "invalid replacement");
    let id = format!("approved-{}", std::process::id());
    let request = json!({"request_id":id, "operation":"finish"}).to_string();
    let (status, body) = api.post(&request);
    assert_eq!(status, 200, "{body}");
    let first: Value = serde_json::from_str(&body).unwrap();
    assert!(matches!(
        first["state"].as_str(),
        Some("running" | "exited")
    ));
    assert_eq!(api.post(&request).0, 200);
    let inventory = f.inventory();
    let sandboxes = inventory["members"][0]["sandboxes"].as_array().unwrap();
    assert_eq!(sandboxes.len(), 1);
    let handle = sandboxes[0]["handle"].as_str().unwrap().to_owned();
    let (member, container) = handle.split_once(':').unwrap();
    f.created
        .borrow_mut()
        .push((member.parse().unwrap(), container.into()));
    assert_eq!(
        api.post(&json!({"request_id":id, "operation":"other"}).to_string())
            .0,
        409
    );
    assert_eq!(
        api.post(&json!({"request_id":id, "operation":"missing"}).to_string())
            .0,
        409
    );
    drop(api);
    // New API and hub processes recover operation and identity solely from Docker.
    f.stop(Signal::TERM);
    f.start();
    approved_operations(&f);
    let api = HttpApi::configured(&f, "hub.sock", true);
    assert_eq!(api.post(&request).0, 200);
    assert_eq!(
        f.inventory()["members"][0]["sandboxes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    if real {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let (status, job) = api.job(&id);
            assert_eq!(status, 200);
            assert!(!job.to_string().contains(TEST_OPERATION_SECRET));
            if job["state"] == "exited" {
                assert_eq!(job["exit_code"], 7);
                break;
            }
            assert!(Instant::now() < end, "job exit watchdog");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    f.ok(&["release", &handle], None);
    assert_eq!(api.job(&id).0, 404);
    // No receipt survives release: the request ID can name another operation.
    assert_eq!(
        api.post(&json!({"request_id":id,"operation":"other"}).to_string())
            .0,
        200
    );
    let placed = f.inventory()["members"][0]["sandboxes"][0].clone();
    let handle = placed["handle"].as_str().unwrap();
    let (member, container) = handle.split_once(':').unwrap();
    f.created
        .borrow_mut()
        .push((member.parse().unwrap(), container.into()));
    f.ok(&["release", handle], None);
}
#[test]
fn api_process_submits_retries_and_recovers_jobs() {
    api_submits_approved_job(false);
}
#[test]
#[ignore = "requires a disposable Docker engine and alpine:3.22"]
fn api_engine_submits_retries_and_observes_exit_code() {
    api_submits_approved_job(true);
}

#[test]
fn api_job_input_is_bounded_strict_and_authenticated_before_hub_calls() {
    let f = Fixture::new(false);
    approved_operations(&f);
    let api = HttpApi::configured(&f, "hub.sock", true);
    let directory = fs::File::open(&f.root).unwrap();
    let trap = std::os::unix::net::UnixListener::bind(format!(
        "/proc/self/fd/{}/hub.sock",
        std::os::fd::AsRawFd::as_raw_fd(&directory)
    ))
    .unwrap();
    fs::set_permissions(f.root.join("hub.sock"), fs::Permissions::from_mode(0o600)).unwrap();
    trap.set_nonblocking(true).unwrap();
    for endpoint in ["/v1/jobs", "/v1/jobs/example", "/v1/operations"] {
        let method = if endpoint == "/v1/jobs" {
            "POST"
        } else {
            "GET"
        };
        assert_eq!(api.raw(&format!("{method} {endpoint} HTTP/1.1\r\nAuthorization: Bearer wrong\r\nContent-Length: 100\r\n\r\n")).0, 401);
    }
    for body in [
        "{",
        "{}",
        r#"{"request_id":"x","operation":"finish","operation":"other"}"#,
        r#"{"request_id":"","operation":"finish"}"#,
        r#"{"request_id":"bad/value","operation":"finish"}"#,
    ] {
        assert_eq!(api.post(body).0, 400);
    }
    let mut invalid = json!({"request_id":"x", "operation":"finish"});
    for field in [
        "image",
        "command",
        "environment",
        "profile",
        "flags",
        "fresh",
        "preferences",
    ] {
        invalid[field] = json!("client-controlled");
        assert_eq!(api.post(&invalid.to_string()).0, 400);
        invalid.as_object_mut().unwrap().remove(field);
    }
    invalid["request_id"] = json!("x".repeat(129));
    assert_eq!(api.post(&invalid.to_string()).0, 400);
    assert_eq!(api.post(&"x".repeat(4097)).0, 413);
    for framing in [
        "Content-Length: 1\r\nContent-Length: 1",
        "Transfer-Encoding: chunked",
        "Content-Length: -1",
        "Content-Length: x",
    ] {
        assert_eq!(api.raw(&format!("POST /v1/jobs HTTP/1.1\r\nAuthorization: Bearer {TEST_API_TOKEN}\r\n{framing}\r\n\r\nx")).0, 400);
    }
    assert_eq!(
        trap.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn api_full_pool_and_unreachable_inventory_never_claim_a_start() {
    let mut f = Fixture::new(false);
    f.write("members.toml", &f.member(1, "active", 1000, 1));
    f.start();
    approved_operations(&f);
    let api = HttpApi::configured(&f, "hub.sock", true);
    assert_eq!(
        api.post(r#"{"request_id":"missing","operation":"missing"}"#)
            .0,
        404
    );
    let placed = f.place("occupied", 100, 33554432);
    let body = r#"{"request_id":"full","operation":"finish"}"#;
    let (status, response) = api.post(body);
    assert_eq!(status, 503);
    assert_eq!(
        serde_json::from_str::<Value>(&response).unwrap()["state"],
        "unknown"
    );
    assert_eq!(
        f.inventory()["members"][0]["sandboxes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    f.release(&placed);
    assert_eq!(api.post(body).0, 200);
    f.write("unavailable", "");
    let job = api.job("full");
    assert_eq!(job.0, 200);
    assert_eq!(job.1["state"], "unknown");
    assert_eq!(job.1["exit_code"], Value::Null);
    let (status, response) = api.post(r#"{"request_id":"outage","operation":"finish"}"#);
    assert_eq!(status, 503);
    assert_eq!(
        serde_json::from_str::<Value>(&response).unwrap()["state"],
        "unknown"
    );
    fs::remove_file(f.root.join("unavailable")).unwrap();
    assert_eq!(
        f.inventory()["members"][0]["sandboxes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let observed = f.inventory()["members"][0]["sandboxes"][0].clone();
    f.ok(&["release", observed["handle"].as_str().unwrap()], None);
    // Start reached the engine, but its confirmation and inventory are lost.
    f.write("unconfirmed-1", "");
    let body = r#"{"request_id":"unconfirmed","operation":"finish"}"#;
    for _ in 0..2 {
        let (status, response) = api.post(body);
        assert_eq!(status, 503);
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap()["state"],
            "unknown"
        );
    }
    fs::remove_file(f.root.join("unavailable-1")).unwrap();
    assert_eq!(api.post(body).0, 200);
    assert_eq!(
        f.inventory()["members"][0]["sandboxes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn api_lost_hub_placement_response_is_reconciled_without_duplicate_work() {
    use std::io::{BufRead, BufReader};
    use std::os::{
        fd::AsRawFd,
        unix::net::{UnixListener, UnixStream},
    };
    let mut f = Fixture::new(false);
    f.start();
    approved_operations(&f);
    let directory = fs::File::open(&f.root).unwrap();
    let base = format!("/proc/self/fd/{}", directory.as_raw_fd());
    let proxy = UnixListener::bind(format!("{base}/proxy.sock")).unwrap();
    fs::set_permissions(f.root.join("proxy.sock"), fs::Permissions::from_mode(0o600)).unwrap();
    proxy.set_nonblocking(true).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping = std::sync::Arc::clone(&stop);
    let worker = std::thread::spawn(move || {
        let _directory = directory;
        let mut lost = false;
        while !stopping.load(Ordering::Acquire) {
            let (mut client, _) = match proxy.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("proxy accept: {error}"),
            };
            client
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut request = String::new();
            BufReader::new(&client).read_line(&mut request).unwrap();
            let mut hub = UnixStream::connect(format!("{base}/hub.sock")).unwrap();
            hub.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            hub.write_all(request.as_bytes()).unwrap();
            let mut response = String::new();
            BufReader::new(hub).read_line(&mut response).unwrap();
            if !lost && serde_json::from_str::<Value>(&request).unwrap()["op"] == "place" {
                lost = true;
            } else {
                let _ = client.write_all(response.as_bytes());
            }
        }
        assert!(lost);
    });
    let api = HttpApi::configured(&f, "proxy.sock", true);
    let body = r#"{"request_id":"lost","operation":"finish"}"#;
    assert_eq!(api.post(body).0, 200);
    assert_eq!(api.post(body).0, 200);
    assert_eq!(
        f.inventory()["members"][0]["sandboxes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    drop(api);
    stop.store(true, Ordering::Release);
    worker.join().unwrap();
}

#[test]
fn api_rejects_invalid_private_operations_without_echoing_values() {
    let f = Fixture::new(false);
    f.write("token", TEST_API_TOKEN);
    let address = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)).to_string();
    let refuse = || {
        let output = f
            .command()
            .args([
                "api",
                "--listen",
                &address,
                "--token-file",
                "token",
                "--operations-file",
                "operations.toml",
                "--socket",
                "hub.sock",
            ])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "homepool: invalid API operations file\n"
        );
    };
    refuse();
    approved_operations(&f);
    let valid = fs::read_to_string(f.root.join("operations.toml")).unwrap();
    for invalid in [
        format!("{valid}\nprofile = 'runner'"),
        valid.replace("version = 1", "version = 2"),
        valid.replace("name = \"other\"", "name = \"finish\""),
        valid.replace("cpu_millicores = 100", "cpu_millicores = 1"),
        valid.replace("memory_bytes = 33554432", "memory_bytes = 0"),
        valid.replace(
            "name = \"finish\"",
            &format!("name = \"{}\"", "a".repeat(60)),
        ),
        valid.replace(
            TEST_OPERATION_SECRET,
            &format!("{TEST_OPERATION_SECRET}\\ninvalid"),
        ),
        format!("{valid}\nunknown = '{TEST_OPERATION_SECRET}'"),
        "x".repeat(1024 * 1024 + 1),
    ] {
        f.write("operations.toml", &invalid);
        refuse();
    }
    f.write("operations.toml", &valid);
    fs::set_permissions(
        f.root.join("operations.toml"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    refuse();
    fs::remove_file(f.root.join("operations.toml")).unwrap();
    f.write("other-operations", &valid);
    std::os::unix::fs::symlink("other-operations", f.root.join("operations.toml")).unwrap();
    refuse();
}

#[test]
fn api_serializes_changed_operation_submissions_and_excludes_another_writer() {
    let mut f = Fixture::new(false);
    f.start();
    approved_operations(&f);
    let api = HttpApi::configured(&f, "hub.sock", true);
    let other = f
        .command()
        .args([
            "api",
            "--listen",
            &api.address.to_string(),
            "--token-file",
            "token",
            "--operations-file",
            "operations.toml",
            "--socket",
            "hub.sock",
        ])
        .output()
        .unwrap();
    assert!(!other.status.success());
    assert!(
        String::from_utf8(other.stderr)
            .unwrap()
            .contains("submission lock")
    );
    std::thread::scope(|scope| {
        let one = scope.spawn(|| api.post(r#"{"request_id":"concurrent","operation":"finish"}"#));
        let two = scope.spawn(|| api.post(r#"{"request_id":"concurrent","operation":"other"}"#));
        let mut statuses = [one.join().unwrap().0, two.join().unwrap().0];
        statuses.sort();
        assert_eq!(statuses, [200, 409]);
    });
    assert_eq!(
        f.inventory()["members"][0]["sandboxes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
