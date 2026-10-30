//! Adapter protocol constants and operational defaults.

pub(crate) const DOCKER_PROGRAM: &str = "docker";
pub(crate) const ENV_STDIN_PATH: &str = "/dev/stdin";
pub(crate) const MANAGED_LABEL: &str = "homepool.managed";
pub(crate) const MEMBER_LABEL: &str = "homepool.member";
pub(crate) const SANDBOX_LABEL: &str = "homepool.sandbox";
pub(crate) const STOP_SECONDS: &str = "10";
pub(crate) const MILLICORES_PER_CPU: u64 = 1_000;
// Docker's env-file scanner limits each line to less than 64 KiB.
pub(crate) const MAX_ENV_LINE_BYTES: usize = 65_535;
pub(crate) const INSPECT_FORMAT: &str =
    r#"{"id":{{json .Id}},"labels":{{json .Config.Labels}},"state":{{json .State.Status}}}"#;
pub(crate) const CAPACITY_FORMAT: &str =
    r#"{"cpus":{{json .NCPU}},"memory_bytes":{{json .MemTotal}}}"#;
