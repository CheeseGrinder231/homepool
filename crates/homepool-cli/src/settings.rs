//! Private member schema and configuration boundary. Core receives only validated types.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use homepool_adapter_docker::{
    BindMount, Endpoint, LaunchProfile, TmpfsMount, VolumeMount, valid_profile_name,
};
use homepool_core::{Capabilities, ComputeBudget, Member, MemberId, MemberState, Pool, Tier};
use toml::{
    Spanned,
    de::{DeTable, DeValue},
};

/// Version of the member-file schema.
pub const SCHEMA_VERSION: u64 = 1;
/// Used only when neither an explicit path nor HOMEPOOL_CONFIG is supplied.
const DEFAULT_CONFIG_PATH: &str = "homepool/members.toml";

/// Resolve one file, without falling back after an invalid or unreadable selection.
/// Inputs are captured by the caller so tests never mutate process environment.
pub fn resolve_config(
    explicit: Option<PathBuf>,
    configured: Option<OsString>,
    xdg: Option<OsString>,
) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return nonempty(path, "--config");
    }
    if let Some(path) = configured {
        return nonempty(PathBuf::from(path), "HOMEPOOL_CONFIG");
    }
    let root = xdg
        .map(PathBuf::from)
        .ok_or("XDG_CONFIG_HOME is unset; supply --config FILE or HOMEPOOL_CONFIG")?;
    if !root.is_absolute() {
        return Err("XDG_CONFIG_HOME must be an absolute, nonempty directory path; supply --config FILE or HOMEPOOL_CONFIG".into());
    }
    Ok(root.join(DEFAULT_CONFIG_PATH))
}

fn nonempty(path: PathBuf, field: &str) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err(format!("{field}: file path must not be empty"));
    }
    Ok(path)
}

pub fn load(path: &Path) -> Result<Pool, String> {
    let source =
        fs::read_to_string(path).map_err(|e| format!("{}:1: <file>: {e}", path.display()))?;
    parse(path, &source)
}

/// Parse with spans retained through validation, then discard all TOML values.
pub fn parse(path: &Path, source: &str) -> Result<Pool, String> {
    Ok(parse_configuration(path, source)?.pool)
}

pub struct Configuration {
    pub pool: Pool,
    pub docker: BTreeMap<MemberId, DockerEndpoint>,
    pub profiles: BTreeMap<MemberId, BTreeMap<String, LaunchProfile>>,
}

#[derive(Clone)]
pub enum DockerEndpoint {
    Unix {
        socket: String,
    },
    Ssh {
        host: String,
        port: Option<u16>,
        remote_socket: Option<String>,
    },
}

impl DockerEndpoint {
    pub fn endpoint(&self) -> Result<Endpoint, String> {
        match self {
            Self::Unix { socket } => Endpoint::unix(socket),
            Self::Ssh {
                host,
                port,
                remote_socket,
            } => Endpoint::ssh(host, *port, remote_socket.as_deref().map(Path::new)),
        }
        .map_err(|e| e.to_string())
    }
}

pub fn parse_configuration(path: &Path, source: &str) -> Result<Configuration, String> {
    let document = Document { path, source };
    let root = DeTable::parse(source).map_err(|e| {
        document.error(
            e.span().map_or(0, |span| span.start),
            "<document>",
            e.message(),
        )
    })?;
    let table = root.get_ref();
    document.fields(table, "", &["version", "members"])?;
    let version = document.required(table, root.span().start, "", "version")?;
    if document.unsigned(version, "version", u64::MAX)? != SCHEMA_VERSION {
        return Err(document.error(
            version.span().start,
            "version",
            "unsupported schema version; expected 1",
        ));
    }
    let members = document.required(table, root.span().start, "", "members")?;
    let entries = members.get_ref().as_array().ok_or_else(|| {
        document.error(
            members.span().start,
            "members",
            "expected an array of member tables",
        )
    })?;
    let mut pool = Pool::default();
    let mut docker = BTreeMap::new();
    let mut profiles = BTreeMap::new();
    for (index, value) in entries.iter().enumerate() {
        let field = format!("members[{index}]");
        let mut member = document.member(value, &field)?;
        if let Some(compute) = value
            .get_ref()
            .as_table()
            .and_then(|t| t.get("capabilities"))
            .and_then(|v| v.get_ref().as_table())
            .and_then(|t| t.get("compute"))
            .and_then(|v| v.get_ref().as_table())
            && let Some(value) = compute.get("profiles")
        {
            let declared =
                document.profiles(value, &format!("{field}.capabilities.compute.profiles"))?;
            member.profiles = declared.keys().cloned().collect();
            profiles.insert(member.id, declared);
        }
        let table = value.get_ref().as_table().expect("validated member table");
        if let Some(value) = table.get("docker") {
            let field = format!("{field}.docker");
            let table = document.table(value, &field)?;
            let string = |key| -> Result<String, String> {
                let v = document.required(table, value.span().start, &field, key)?;
                v.get_ref().as_str().map(str::to_owned).ok_or_else(|| {
                    document.error(
                        v.span().start,
                        &field_name(&field, key),
                        "expected a string",
                    )
                })
            };
            let endpoint = if table.contains_key("socket") {
                document.fields(table, &field, &["socket"])?;
                DockerEndpoint::Unix {
                    socket: string("socket")?,
                }
            } else {
                document.fields(table, &field, &["host", "port", "remote_socket"])?;
                DockerEndpoint::Ssh {
                    host: string("host")?,
                    port: table
                        .get("port")
                        .map(|v| {
                            document
                                .unsigned(v, &field_name(&field, "port"), u16::MAX.into())
                                .map(|p| p as u16)
                        })
                        .transpose()?,
                    remote_socket: table
                        .get("remote_socket")
                        .map(|_| string("remote_socket"))
                        .transpose()?,
                }
            };
            endpoint.endpoint().map_err(|_| {
                document.error(value.span().start, &field, "invalid Docker endpoint")
            })?;
            docker.insert(member.id, endpoint);
        }
        pool.add_member(member).map_err(|_| {
            let table = value.get_ref().as_table().expect("validated member table");
            document.error(
                table.get("id").expect("validated id").span().start,
                &format!("{field}.id"),
                "duplicate member id",
            )
        })?;
    }
    Ok(Configuration {
        pool,
        docker,
        profiles,
    })
}

struct Document<'a> {
    path: &'a Path,
    source: &'a str,
}

impl Document<'_> {
    fn error(&self, offset: usize, field: &str, message: &str) -> String {
        let line = self.source.as_bytes()[..offset.min(self.source.len())]
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count()
            + 1;
        format!("{}:{line}: {field}: {message}", self.path.display())
    }

    fn fields(&self, table: &DeTable<'_>, prefix: &str, allowed: &[&str]) -> Result<(), String> {
        for key in table.keys() {
            if !allowed.contains(&key.get_ref().as_ref()) {
                return Err(self.error(
                    key.span().start,
                    &field_name(prefix, key.get_ref()),
                    "unknown field",
                ));
            }
        }
        Ok(())
    }

    fn required<'a, 'i>(
        &self,
        table: &'a DeTable<'i>,
        start: usize,
        prefix: &str,
        key: &str,
    ) -> Result<&'a Spanned<DeValue<'i>>, String> {
        table
            .get(key)
            .ok_or_else(|| self.error(start, &field_name(prefix, key), "missing required field"))
    }

    fn table<'a, 'i>(
        &self,
        value: &'a Spanned<DeValue<'i>>,
        field: &str,
    ) -> Result<&'a DeTable<'i>, String> {
        value
            .get_ref()
            .as_table()
            .ok_or_else(|| self.error(value.span().start, field, "expected a table"))
    }

    fn unsigned(&self, value: &Spanned<DeValue<'_>>, field: &str, max: u64) -> Result<u64, String> {
        value
            .get_ref()
            .as_integer()
            .and_then(|v| u64::from_str_radix(v.as_str(), v.radix()).ok())
            .filter(|&v| v <= max)
            .ok_or_else(|| {
                self.error(
                    value.span().start,
                    field,
                    &format!("expected an integer from 0 to {max}"),
                )
            })
    }

    fn member(&self, value: &Spanned<DeValue<'_>>, field: &str) -> Result<Member, String> {
        let table = self.table(value, field)?;
        self.fields(
            table,
            field,
            &["id", "tier", "state", "capabilities", "docker"],
        )?;
        let get = |key| self.required(table, value.span().start, field, key);
        let id = MemberId(self.unsigned(get("id")?, &field_name(field, "id"), u64::MAX)?);
        let tier_value = get("tier")?;
        let tier = match tier_value.get_ref().as_str() {
            Some("core") => Tier::Core,
            Some("burst") => Tier::Burst,
            _ => {
                return Err(self.error(
                    tier_value.span().start,
                    &field_name(field, "tier"),
                    "expected core or burst",
                ));
            }
        };
        let state_value = get("state")?;
        let state = match state_value.get_ref().as_str() {
            Some("active") => MemberState::Active,
            Some("draining") => MemberState::Draining,
            Some("unreachable") => MemberState::Unreachable,
            _ => {
                return Err(self.error(
                    state_value.span().start,
                    &field_name(field, "state"),
                    "expected active, draining or unreachable",
                ));
            }
        };
        let capabilities =
            self.capabilities(get("capabilities")?, &field_name(field, "capabilities"))?;
        Ok(Member {
            id,
            tier,
            state,
            capabilities,
            profiles: Default::default(),
        })
    }

    fn capabilities(
        &self,
        value: &Spanned<DeValue<'_>>,
        field: &str,
    ) -> Result<Capabilities, String> {
        let table = self.table(value, field)?;
        self.fields(table, field, &["compute", "storage_bytes"])?;
        let storage_bytes = table
            .get("storage_bytes")
            .map(|v| self.unsigned(v, &field_name(field, "storage_bytes"), u64::MAX))
            .transpose()?
            .unwrap_or(0);
        let compute = table
            .get("compute")
            .map(|v| self.compute(v, &field_name(field, "compute")))
            .transpose()?;
        Ok(Capabilities {
            compute,
            storage_bytes,
        })
    }

    fn string(
        &self,
        table: &DeTable<'_>,
        start: usize,
        field: &str,
        key: &str,
    ) -> Result<String, String> {
        let value = self.required(table, start, field, key)?;
        value.get_ref().as_str().map(str::to_owned).ok_or_else(|| {
            self.error(
                value.span().start,
                &field_name(field, key),
                "expected a string",
            )
        })
    }

    fn boolean(&self, table: &DeTable<'_>, field: &str, key: &str) -> Result<bool, String> {
        table
            .get(key)
            .map(|v| {
                v.get_ref().as_bool().ok_or_else(|| {
                    self.error(
                        v.span().start,
                        &field_name(field, key),
                        "expected a boolean",
                    )
                })
            })
            .transpose()
            .map(|v| v.unwrap_or(false))
    }

    fn profiles(
        &self,
        value: &Spanned<DeValue<'_>>,
        field: &str,
    ) -> Result<BTreeMap<String, LaunchProfile>, String> {
        let mut profiles = BTreeMap::new();
        for (name, value) in self.table(value, field)?.iter() {
            let field = field_name(field, name.get_ref());
            if !valid_profile_name(name.get_ref()) {
                return Err(self.error(name.span().start, &field, "invalid profile name"));
            }
            let table = self.table(value, &field)?;
            self.fields(
                table,
                &field,
                &[
                    "privileged",
                    "pids_limit",
                    "shm_bytes",
                    "memory_swap_bytes",
                    "tmpfs",
                    "volumes",
                    "binds",
                ],
            )?;
            let limit = |key| {
                table
                    .get(key)
                    .map(|v| self.unsigned(v, &field_name(&field, key), i64::MAX as u64))
                    .transpose()
            };
            let mut profile = LaunchProfile {
                privileged: self.boolean(table, &field, "privileged")?,
                pids_limit: limit("pids_limit")?,
                shm_bytes: limit("shm_bytes")?,
                memory_swap_bytes: limit("memory_swap_bytes")?,
                ..Default::default()
            };
            for key in ["tmpfs", "volumes", "binds"] {
                if let Some(value) = table.get(key) {
                    let field = field_name(&field, key);
                    let array = value.get_ref().as_array().ok_or_else(|| {
                        self.error(value.span().start, &field, "expected an array")
                    })?;
                    for (index, value) in array.iter().enumerate() {
                        let field = format!("{field}[{index}]");
                        let table = self.table(value, &field)?;
                        let allowed: &[&str] = match key {
                            "tmpfs" => &["target", "options"],
                            "volumes" => &["name", "target", "read_only"],
                            _ => &["source", "target", "read_only"],
                        };
                        self.fields(table, &field, allowed)?;
                        let string = |key| self.string(table, value.span().start, &field, key);
                        let target = string("target")?;
                        match key {
                            "tmpfs" => profile.tmpfs.push(TmpfsMount {
                                target,
                                options: string("options")?,
                            }),
                            "volumes" => profile.volumes.push(VolumeMount {
                                target,
                                name: string("name")?,
                                read_only: self.boolean(table, &field, "read_only")?,
                            }),
                            _ => profile.binds.push(BindMount {
                                target,
                                source: string("source")?,
                                read_only: self.boolean(table, &field, "read_only")?,
                            }),
                        }
                    }
                }
            }
            profile
                .validate()
                .map_err(|e| self.error(value.span().start, &field, &e.to_string()))?;
            profiles.insert(name.get_ref().to_string(), profile);
        }
        Ok(profiles)
    }

    fn compute(&self, value: &Spanned<DeValue<'_>>, field: &str) -> Result<ComputeBudget, String> {
        let table = self.table(value, field)?;
        self.fields(
            table,
            field,
            &[
                "cpu_millicores",
                "memory_bytes",
                "sandbox_slots",
                "profiles",
            ],
        )?;
        let get = |key, max| {
            let v = self.required(table, value.span().start, field, key)?;
            self.unsigned(v, &field_name(field, key), max)
        };
        Ok(ComputeBudget {
            cpu_millicores: get("cpu_millicores", u64::MAX)?,
            memory_bytes: get("memory_bytes", u64::MAX)?,
            sandbox_slots: get("sandbox_slots", u32::MAX.into())? as u32,
        })
    }
}

fn field_name(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.into()
    } else {
        format!("{prefix}.{key}")
    }
}

/// Socket path precedence mirrors the member file, without a home fallback.
pub fn resolve_socket(
    explicit: Option<PathBuf>,
    configured: Option<OsString>,
    xdg: Option<OsString>,
) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return nonempty(path, "--socket");
    }
    if let Some(path) = configured {
        return nonempty(PathBuf::from(path), "HOMEPOOL_SOCKET");
    }
    let root = xdg
        .map(PathBuf::from)
        .ok_or("XDG_RUNTIME_DIR is unset; supply --socket FILE or HOMEPOOL_SOCKET")?;
    if !root.is_absolute() {
        return Err("XDG_RUNTIME_DIR must be an absolute, nonempty directory path".into());
    }
    Ok(root.join(DEFAULT_SOCKET_PATH))
}

pub const DEFAULT_SOCKET_PATH: &str = "homepool/hub.sock";
pub const HUB_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
pub const API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
pub const API_SIZE_LIMIT: usize = 1024 * 1024;
/// Last lines retained independently for stdout and stderr.
pub const LOG_DEFAULT_TAIL: usize = 100;
pub const LOG_MAX_TAIL: usize = 1000;
/// Combined raw bytes; each pipe gets half. Six bytes per input byte covers
/// worst-case JSON escaping and lossy UTF-8, leaving space for the envelope.
pub const LOG_BYTES: usize = (API_SIZE_LIMIT - 1024) / 6;
pub const CPU_QUANTUM: u64 = 10;

/// Reserve response time inside the same client-supplied request deadline.
pub const RESPONSE_MARGIN: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Clone, Copy)]
pub struct Timing {
    pub operation: std::time::Duration,
    pub request: std::time::Duration,
}
impl Timing {
    pub fn load() -> Result<Self, String> {
        fn duration(
            key: &str,
            default: std::time::Duration,
        ) -> Result<std::time::Duration, String> {
            let duration = match std::env::var(key) {
                Ok(value) => std::time::Duration::from_millis(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("{key} must be positive milliseconds"))?,
                ),
                Err(std::env::VarError::NotPresent) => default,
                Err(_) => return Err(format!("invalid {key}")),
            };
            if duration.is_zero() || std::time::Instant::now().checked_add(duration).is_none() {
                return Err(format!("invalid {key}"));
            }
            Ok(duration)
        }
        let timing = Self {
            operation: duration(
                "HOMEPOOL_OPERATION_TIMEOUT_MS",
                homepool_adapter_docker::Docker::DEFAULT_OPERATION_TIMEOUT,
            )?,
            request: duration("HOMEPOOL_REQUEST_TIMEOUT_MS", API_TIMEOUT)?,
        };
        if timing.request <= RESPONSE_MARGIN {
            return Err("request timeout must exceed the response margin (100 ms)".into());
        }
        Ok(timing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_file_v1_contract() {
        assert_eq!(SCHEMA_VERSION, 1);
        let path = Path::new("members.toml");
        assert!(parse(path, "version = 1\nmembers = []").is_ok());
        assert!(parse(path, "version = 2\nmembers = []").is_err());
    }
}

#[cfg(test)]
mod profile_tests {
    use super::*;
    fn source(profile: &str) -> String {
        format!(
            "version = 1\n[[members]]\nid = 1\ntier = 'core'\nstate = 'active'\n[members.capabilities.compute]\ncpu_millicores = 1000\nmemory_bytes = 268435456\nsandbox_slots = 2\n[members.capabilities.compute.profiles.runner]\n{profile}\n"
        )
    }

    #[test]
    fn profiles_survive_canonical_member_updates_and_status_contains_only_names() {
        let original = source(
            r#"privileged = true
pids_limit = 128
shm_bytes = 33554432
memory_swap_bytes = 134217728
tmpfs = [{target = "/scratch", options = "rw,size=16m"}]
volumes = [{name = "cache", target = "/cache", read_only = true}]
binds = [{source = "/srv/input", target = "/input", read_only = true}]"#,
        );
        let path = Path::new("members.toml");
        let original = format!(
            "{original}\n[members.capabilities.compute.profiles.\"build.debug\"]\n[members.capabilities.compute.profiles.empty]\n"
        );
        let config = parse_configuration(path, &original).unwrap();
        let encoded = crate::private::encode(&config);
        let restored = parse_configuration(path, &encoded).unwrap();
        assert_eq!(crate::private::encode(&restored), encoded);
        let status: serde_json::Value =
            serde_json::from_str(&crate::status::render(&restored.pool, true, None).unwrap())
                .unwrap();
        assert_eq!(
            status["members"][0]["capabilities"]["compute"]["profiles"],
            serde_json::json!(["build.debug", "empty", "runner"])
        );
        assert!(!restored.profiles[&MemberId(1)]["empty"].privileged);
        let p = &restored.profiles[&MemberId(1)]["runner"];
        assert!(p.privileged);
        assert_eq!(p.pids_limit, Some(128));
        assert_eq!(p.shm_bytes, Some(33554432));
        assert_eq!(p.memory_swap_bytes, Some(134217728));
        assert_eq!(p.tmpfs[0].options, "rw,size=16m");
        assert_eq!(p.volumes[0].name, "cache");
        assert!(p.volumes[0].read_only);
        assert_eq!(p.binds[0].source, "/srv/input");
        assert!(p.binds[0].read_only);
        for json in [true, false] {
            let output = crate::status::render(&restored.pool, json, None).unwrap();
            assert!(output.contains("runner"));
            for private in [
                "privileged",
                "pids_limit",
                "/srv/input",
                "/cache",
                "/scratch",
            ] {
                assert!(!output.contains(private));
            }
        }
    }
}

/// HTTP request line plus headers, and number of parsed fields.
pub const HTTP_HEADER_BYTES: usize = 8192;
pub const HTTP_HEADER_COUNT: usize = 32;
/// Absolute header deadline and response-write allowance (no keep-alive).
pub const HTTP_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Bound concurrent connection threads, including hub calls.
pub const HTTP_CONNECTIONS: usize = 32;
/// Maximum bearer token bytes, excluding an optional final newline.
pub const HTTP_TOKEN_BYTES: usize = 256;

/// Maximum JSON job body and private operations document bytes.
pub const HTTP_BODY_BYTES: usize = 4096;
pub const OPERATIONS_BYTES: usize = API_SIZE_LIMIT;
/// SHA-256 request digest plus namespace and operation must fit a 128-byte identity.
pub const OPERATION_NAME_BYTES: usize = 59;

/// Bounded unread suffix consumed after sending a response, sharing its deadline.
pub const HTTP_DRAIN_BYTES: usize = HTTP_HEADER_BYTES + HTTP_BODY_BYTES;
