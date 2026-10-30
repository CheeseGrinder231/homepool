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
        name: format!("homepool-3-{identity}-cache"),
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
