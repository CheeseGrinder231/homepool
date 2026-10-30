//! Opt-in tests requiring a local Unix Docker engine and alpine:3.22.
//! CI runs these explicitly; an unavailable engine is a failure, never a skip.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use homepool_adapter_docker::{BindMount, Docker, Endpoint, Environment, Error, SandboxSpec};
use homepool_core::MemberId;
use serde_json::Value;

fn endpoint() -> Endpoint {
    Endpoint::unix(
        std::env::var_os("HOMEPOOL_TEST_DOCKER_SOCKET")
            .unwrap_or_else(|| "/var/run/docker.sock".into()),
    )
    .unwrap()
}

fn engine() -> Docker {
    Docker::new(endpoint(), MemberId(4))
}

fn unique() -> String {
    format!(
        "test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn spec() -> SandboxSpec {
    SandboxSpec {
        sandbox_id: unique(),
        image: "alpine:3.22".into(),
        cpu_millicores: 500,
        memory_bytes: 64 * 1024 * 1024,
        mounts: vec![],
        environment: Environment::default(),
        // Handle stop immediately, without waiting for the stop timeout.
        command: vec![
            "sh".into(),
            "-c".into(),
            "trap 'exit 0' TERM; while :; do sleep 1 & wait $!; done".into(),
        ],
    }
}

fn cli(args: &[&str]) -> Vec<u8> {
    let output = Command::new("docker")
        .arg("--host")
        .arg(endpoint().as_str())
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_TLS")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "test Docker command failed; diagnostics redacted"
    );
    output.stdout
}

struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        // Best-effort cleanup even when an assertion panics.
        let _ = Command::new("docker")
            .arg("--host")
            .arg(endpoint().as_str())
            .env_remove("DOCKER_CONTEXT")
            .env_remove("DOCKER_HOST")
            .env_remove("DOCKER_TLS")
            .env_remove("DOCKER_TLS_VERIFY")
            .env_remove("DOCKER_CERT_PATH")
            .args(["container", "rm", "--force", &self.0])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = PathBuf::from(
            std::env::var_os("TMPDIR").expect("TMPDIR must name the test scratch directory"),
        )
        .join(unique());
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "requires a local Docker engine and alpine:3.22"]
fn capacity_and_full_lifecycle_with_limits_mounts_and_environment() {
    let docker = engine();
    let capacity = docker.capacity().unwrap();
    assert!(capacity.cpu_millicores >= 1000);
    assert!(capacity.memory_bytes > 0);

    let scratch = Scratch::new();
    fs::write(scratch.0.join("input"), "mounted value").unwrap();
    let mut request = spec();
    request
        .environment
        .insert("REGISTRATION_TOKEN", "dummy-secret='quoted' = value")
        .unwrap();
    request.environment.insert("EMPTY", "").unwrap();
    // Exercise concurrent pipe writing, beyond the pipe buffer's capacity.
    for i in 0..32 {
        request
            .environment
            .insert(&format!("BULK_{i}"), &"x".repeat(4096))
            .unwrap();
    }
    request.mounts.push(BindMount {
        source: scratch.0.to_str().unwrap().into(),
        target: "/input".into(),
        read_only: true,
    });
    let sandbox = docker.start(&request).unwrap();
    let _cleanup = Container(sandbox.container_id.clone());
    assert_eq!(sandbox.state, "running");
    assert_eq!(sandbox.sandbox_id, request.sandbox_id);
    assert!(docker.list().unwrap().contains(&sandbox));

    let details: Value =
        serde_json::from_slice(&cli(&["container", "inspect", &sandbox.container_id])).unwrap();
    let details = &details[0];
    assert_eq!(details["HostConfig"]["NanoCpus"], 500_000_000_u64);
    assert_eq!(details["HostConfig"]["Memory"], request.memory_bytes);
    assert_eq!(details["Config"]["Labels"]["homepool.managed"], "true");
    assert_eq!(details["Config"]["Labels"]["homepool.member"], "4");
    assert!(
        !details["Config"]["Labels"]
            .to_string()
            .contains("dummy-secret")
    );
    // Verify values inside the sandbox without passing their contents as args.
    let result = cli(&[
        "container",
        "exec",
        &sandbox.container_id,
        "sh",
        "-eu",
        "-c",
        "printf '%s\\n' \"$REGISTRATION_TOKEN\"; cat /input/input; test \"${EMPTY+x}\" = x; test -z \"$EMPTY\"; test ${#BULK_31} -eq 4096",
    ]);
    assert_eq!(result, b"dummy-secret='quoted' = value\nmounted value");
    assert_eq!(details["Mounts"][0]["RW"], false);

    docker.stop(&sandbox.container_id).unwrap();
    let stopped = docker
        .list()
        .unwrap()
        .into_iter()
        .find(|item| item.container_id == sandbox.container_id)
        .unwrap();
    assert_eq!(stopped.state, "exited");
    docker.remove(&sandbox.container_id).unwrap();
    assert!(
        !docker
            .list()
            .unwrap()
            .iter()
            .any(|item| item.container_id == sandbox.container_id)
    );
}

#[test]
#[ignore = "requires a local Docker engine and alpine:3.22"]
fn foreign_members_and_unmanaged_containers_are_protected() {
    let sandbox = engine().start(&spec()).unwrap();
    let _cleanup = Container(sandbox.container_id.clone());
    let foreign = Docker::new(endpoint(), MemberId(5));
    assert!(
        !foreign
            .list()
            .unwrap()
            .iter()
            .any(|item| item.container_id == sandbox.container_id)
    );
    assert!(matches!(
        foreign.stop(&sandbox.container_id),
        Err(Error::NotManaged)
    ));
    assert!(matches!(
        foreign.remove(&sandbox.container_id),
        Err(Error::NotManaged)
    ));

    let id = String::from_utf8(cli(&[
        "container",
        "create",
        "--label",
        "homepool.member=4",
        "--label",
        "homepool.sandbox=unmanaged",
        "alpine:3.22",
        "true",
    ]))
    .unwrap()
    .trim()
    .to_owned();
    let _unmanaged = Container(id.clone());
    assert!(
        !engine()
            .list()
            .unwrap()
            .iter()
            .any(|item| item.container_id == id)
    );
    assert!(matches!(engine().stop(&id), Err(Error::NotManaged)));
    assert!(matches!(engine().remove(&id), Err(Error::NotManaged)));
}

#[test]
#[ignore = "requires a local Docker engine and alpine:3.22"]
fn failed_start_rolls_back_the_created_container() {
    let mut request = spec();
    request.command = vec!["/no-such-program".into()];
    let error = engine().start(&request).unwrap_err();
    let Error::StartFailed {
        container_id,
        start_error,
        cleanup_error,
        outcome_uncertain,
    } = error
    else {
        panic!("expected a start failure");
    };
    let _cleanup = Container(container_id.clone());
    assert!(matches!(
        *start_error,
        Error::Command {
            operation: "start",
            ..
        }
    ));
    assert!(cleanup_error.is_none());
    assert!(!outcome_uncertain);
    assert!(
        !engine()
            .list()
            .unwrap()
            .iter()
            .any(|item| item.container_id == container_id)
    );
}

#[test]
#[ignore = "requires a local Docker engine and alpine:3.22"]
fn restart_inventory_restores_differently_sized_running_sandboxes() {
    use homepool_core::{
        Capabilities, ComputeBudget, Member, MemberState, Pool, Reservation, SandboxRequest, Tier,
    };
    use std::collections::BTreeMap;
    let member = Member {
        id: MemberId(4),
        tier: Tier::Core,
        state: MemberState::Active,
        capabilities: Capabilities {
            compute: Some(ComputeBudget {
                cpu_millicores: 4000,
                memory_bytes: 256 * 1024 * 1024,
                sandbox_slots: 4,
            }),
            storage_bytes: 0,
        },
    };
    let docker = engine();
    let mut original = Pool::default();
    original.add_member(member.clone()).unwrap();
    let mut mapping = BTreeMap::new();
    let mut cleanup = Vec::new();
    for (cpu_millicores, memory_bytes) in [(500, 64 * 1024 * 1024), (1250, 96 * 1024 * 1024)] {
        let mut request = spec();
        request.cpu_millicores = cpu_millicores;
        request.memory_bytes = memory_bytes;
        let reservation = original
            .reserve(&SandboxRequest {
                cpu_millicores,
                memory_bytes,
                preferences: vec![],
            })
            .unwrap();
        let sandbox = docker.start(&request).unwrap();
        cleanup.push(Container(sandbox.container_id.clone()));
        mapping.insert(sandbox.container_id, reservation);
    }
    // Reconstruct from a fresh adapter and inventory, retaining the caller's
    // handle-to-reservation mapping. Admission stays closed while rebuilding.
    let mut restarted = Pool::default();
    restarted
        .add_member(Member {
            state: MemberState::Draining,
            ..member
        })
        .unwrap();
    let inventory = engine().list().unwrap();
    for sandbox in inventory {
        let Some(saved) = mapping.get(&sandbox.container_id) else {
            continue;
        };
        assert_eq!(sandbox.state, "running");
        let budget = ComputeBudget {
            cpu_millicores: sandbox.cpu_millicores.expect("bounded CPU"),
            memory_bytes: sandbox.memory_bytes.expect("bounded memory"),
            sandbox_slots: 1,
        };
        assert_eq!(budget, saved.budget);
        assert_eq!(
            restarted.import_allocation(Reservation { budget, ..*saved }),
            Ok(*saved)
        );
    }
    assert_eq!(
        restarted.reservations().copied().collect::<Vec<_>>(),
        original.reservations().copied().collect::<Vec<_>>()
    );
    assert_eq!(
        restarted.reserve(&SandboxRequest::default()),
        Err(homepool_core::Error::NoCapacity)
    );
    restarted
        .set_state(MemberId(4), MemberState::Active)
        .unwrap();
    assert_eq!(
        restarted.free_budget(MemberId(4)),
        original.free_budget(MemberId(4))
    );
    for reservation in mapping.values() {
        assert_eq!(restarted.release(reservation.id), Ok(*reservation));
    }
}

#[test]
#[ignore = "requires a local Docker engine and alpine:3.22"]
fn quota_only_default_period_supports_inventory_and_lifecycle() {
    let sandbox_id = unique();
    let label = format!("homepool.sandbox={sandbox_id}");
    let id = String::from_utf8(cli(&[
        "container",
        "create",
        "--cpu-quota",
        "50000",
        "--memory",
        "67108864",
        "--label",
        "homepool.managed=true",
        "--label",
        "homepool.member=4",
        "--label",
        &label,
        "alpine:3.22",
        "sleep",
        "60",
    ]))
    .unwrap()
    .trim()
    .to_owned();
    let _cleanup = Container(id.clone());
    cli(&["container", "start", &id]);
    let details: Value = serde_json::from_slice(&cli(&["container", "inspect", &id])).unwrap();
    assert_eq!(details[0]["HostConfig"]["NanoCpus"], 0);
    assert_eq!(details[0]["HostConfig"]["CpuQuota"], 50_000);
    assert_eq!(details[0]["HostConfig"]["CpuPeriod"], 0);
    let docker = engine();
    let observed = docker
        .list()
        .unwrap()
        .into_iter()
        .find(|item| item.container_id == id)
        .unwrap();
    assert_eq!(observed.sandbox_id, sandbox_id);
    assert_eq!(observed.state, "running");
    assert_eq!(observed.cpu_millicores, Some(500));
    assert_eq!(observed.memory_bytes, Some(67_108_864));
    docker.stop(&id).unwrap();
    assert_eq!(
        docker
            .list()
            .unwrap()
            .into_iter()
            .find(|item| item.container_id == id)
            .unwrap()
            .state,
        "exited"
    );
    docker.remove(&id).unwrap();
    assert!(
        !docker
            .list()
            .unwrap()
            .iter()
            .any(|item| item.container_id == id)
    );
}

// Read the applied controller value, not Docker's requested HostConfig. Private
// cgroup namespaces expose the container's own limit at the controller root.
fn applied_cpu_limit(id: &str) -> Option<(u64, u64)> {
    let output = cli(&[
        "container",
        "exec",
        id,
        "sh",
        "-eu",
        "-c",
        r#"
        if [ -r /sys/fs/cgroup/cpu.max ]; then
            cat /sys/fs/cgroup/cpu.max
        else
            for base in /sys/fs/cgroup/cpu /sys/fs/cgroup/cpu,cpuacct /sys/fs/cgroup/cpuacct,cpu /sys/fs/cgroup; do
                if [ -r "$base/cpu.cfs_quota_us" ] && [ -r "$base/cpu.cfs_period_us" ]; then
                    cat "$base/cpu.cfs_quota_us" "$base/cpu.cfs_period_us"
                    exit 0
                fi
            done
            printf 'unavailable\n'
        fi
    "#,
    ]);
    let text = std::str::from_utf8(&output).unwrap();
    if text.trim() == "unavailable" {
        return None;
    }
    let values: Vec<u64> = text
        .split_whitespace()
        .map(|value| {
            value
                .parse()
                .expect("applied CPU quota and period must be finite integers")
        })
        .collect();
    assert_eq!(values.len(), 2);
    assert!(values[0] > 0 && values[1] > 0);
    Some((values[0], values[1]))
}

#[test]
#[ignore = "requires a local Docker engine and alpine:3.22"]
fn inventory_cpu_bounds_cover_applied_cgroup_limits() {
    for (case, limits, expected) in [
        ("quota-default", vec!["--cpu-quota", "50001"], 510_u64),
        (
            "quota-explicit",
            vec!["--cpu-quota", "25001", "--cpu-period", "200000"],
            130,
        ),
        ("nano-cpus", vec!["--cpus", "0.501"], 510),
    ] {
        let label = format!("homepool.sandbox={}", unique());
        let mut args = vec![
            "container",
            "create",
            "--cgroupns",
            "private",
            "--memory",
            "67108864",
            "--label",
            "homepool.managed=true",
            "--label",
            "homepool.member=4",
            "--label",
            &label,
        ];
        args.extend(limits);
        args.extend(["alpine:3.22", "sleep", "300"]);
        let id = String::from_utf8(cli(&args)).unwrap().trim().to_owned();
        let _cleanup = Container(id.clone());
        cli(&["container", "start", &id]);
        let reported = engine()
            .list()
            .unwrap()
            .into_iter()
            .find(|item| item.container_id == id)
            .unwrap()
            .cpu_millicores
            .unwrap();
        assert_eq!(reported, expected);
        let Some((quota, period)) = applied_cpu_limit(&id) else {
            // Make a runner layout limitation explicit in CI logs and the PR
            // validation report, rather than pretending HostConfig proves it.
            println!("APPLIED_CPU_CHECK unavailable: {case}; controller files are not exposed");
            continue;
        };
        assert!(
            u128::from(reported) * u128::from(period) >= u128::from(quota) * 1000,
            "{case}: reported CPU {reported} must cover applied quota {quota}/{period}"
        );
        println!(
            "APPLIED_CPU_CHECK verified: {case}; reported={reported}, quota={quota}, period={period}"
        );
    }
}
