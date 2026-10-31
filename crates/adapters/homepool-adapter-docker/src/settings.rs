//! Adapter protocol constants and operational defaults.

use std::time::Duration;

/// End-to-end limit for each public adapter operation, including all CLI calls.
pub(crate) const OPERATION_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum idle wait between nonblocking pipe and child-status checks.
pub(crate) const TRANSPORT_POLL_INTERVAL: Duration = Duration::from_millis(5);

pub(crate) const DOCKER_PROGRAM: &str = "docker";
pub(crate) const ENV_STDIN_PATH: &str = "/dev/stdin";
pub(crate) const MANAGED_LABEL: &str = "homepool.managed";
pub(crate) const MEMBER_LABEL: &str = "homepool.member";
pub(crate) const SANDBOX_LABEL: &str = "homepool.sandbox";
pub(crate) const STOP_SECONDS: &str = "10";
pub(crate) const MILLICORES_PER_CPU: u64 = 1_000;
/// Runtime systemd quota translation rounds fractional CPU percentages upward.
/// One percent of a CPU is 10 millicores: use this quantum for conservative
/// inventory bounds and require aligned starts so reserved and applied CPU agree.
pub(crate) const CPU_LIMIT_QUANTUM_MILLICORES: u64 = 10;
/// Docker's zero HostConfig.CpuPeriod selects the Linux CFS default: 100 ms,
/// expressed in microseconds, including when only --cpu-quota is specified.
pub(crate) const DEFAULT_CPU_PERIOD_MICROS: u64 = 100_000;
// Docker's env-file scanner limits each line to less than 64 KiB.
pub(crate) const MAX_ENV_LINE_BYTES: usize = 65_535;
pub(crate) const INSPECT_FORMAT: &str = r#"{"id":{{json .Id}},"labels":{{json .Config.Labels}},"state":{{json .State.Status}},"exit_code":{{json .State.ExitCode}},"limits":{"nano_cpus":{{json .HostConfig.NanoCpus}},"cpu_quota":{{json .HostConfig.CpuQuota}},"cpu_period":{{json .HostConfig.CpuPeriod}},"memory_bytes":{{json .HostConfig.Memory}}}}"#;
pub(crate) const CAPACITY_FORMAT: &str =
    r#"{"cpus":{{json .NCPU}},"memory_bytes":{{json .MemTotal}}}"#;

/// Keep local volume directory components within 255 bytes even with a u64
/// member ID (20 digits), a 128-byte sandbox identity, the ID dash and the
/// unambiguous suffix dot: 255 - (9 + 20 + 1 + 128 + 1) = 96.
pub(crate) const MAX_VOLUME_SUFFIX_BYTES: usize = 96;

pub(crate) const ID_FORMAT: &str = "{{json .Id}}";
