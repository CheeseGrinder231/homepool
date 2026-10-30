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
            "/run/member/docker.sock".into()
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
            .env_remove("XDG_RUNTIME_DIR");
        if !self.real {
            c.env(
                "PATH",
                format!("{}:{}", self.root.display(), std::env::var("PATH").unwrap()),
            )
            .env("HOMEPOOL_FAKE_STATE", &self.root);
        }
        c
    }
    fn start(&mut self) {
        self.hub = Some(
            self.command()
                .args(["hub", "--config", "members.toml", "--socket", "hub.sock"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                self.hub.as_mut().unwrap().try_wait().unwrap().is_none(),
                "hub exited during startup"
            );
            if self.root.join("hub.sock").exists()
                && self.run(&["inventory", "--json"], None).status.success()
            {
                break;
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
        let result = self.ok(&["place", "--json"], Some(json!({"sandbox_id": name, "image": "alpine:3.22", "cpu_millicores": cpu, "memory_bytes": memory, "command": ["sleep", "300"]})));
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
    assert!(
        !f.command()
            .args(["hub", "--config", "members.toml", "--socket", "hub.sock"])
            .output()
            .unwrap()
            .status
            .success()
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
