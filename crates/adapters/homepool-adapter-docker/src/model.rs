use std::{collections::BTreeMap, fmt};

use crate::{error::Error, settings::MAX_ENV_LINE_BYTES};

/// Environment values, including secrets. Debug output always redacts values.
/// Values travel only through the Docker CLI's stdin, never its arguments.
/// Docker stores these in container configuration, readable by engine admins.
#[derive(Default)]
pub struct Environment(BTreeMap<String, String>);

impl Environment {
    /// Inserts an explicit value. No values are inherited from the process.
    /// Keys use portable environment-variable syntax; values must be single-line,
    /// without CR or NUL. Each `KEY=value` line must fit Docker's env-file limit.
    pub fn insert(&mut self, key: &str, value: &str) -> Result<(), Error> {
        if key.is_empty()
            || !key
                .bytes()
                .enumerate()
                .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
            || value.contains(['\n', '\r', '\0'])
            || key.len().saturating_add(value.len()).saturating_add(2) > MAX_ENV_LINE_BYTES
        {
            return Err(Error::InvalidInput("environment entry"));
        }
        self.0.insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    pub(crate) fn stdin_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (key, value) in &self.0 {
            bytes.extend_from_slice(key.as_bytes());
            bytes.push(b'=');
            bytes.extend_from_slice(value.as_bytes());
            bytes.push(b'\n');
        }
        bytes
    }
}

impl fmt::Debug for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Environment")
            .field("entries", &self.0.len())
            .finish_non_exhaustive()
    }
}

/// A bind mount. The source belongs to the engine's machine, even over SSH.
#[derive(Clone, Debug)]
pub struct BindMount {
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

/// A sandbox to create and start. Image, identity, mount paths, and command must
/// contain no secrets; place secret values in `environment` instead.
#[derive(Debug)]
pub struct SandboxSpec {
    /// Nonsecret logical identity, recorded in `homepool.sandbox`.
    pub sandbox_id: String,
    pub image: String,
    /// Nonzero CPU limit in millicores.
    pub cpu_millicores: u64,
    /// Nonzero memory limit in bytes (subject to engine minimums).
    pub memory_bytes: u64,
    pub mounts: Vec<BindMount>,
    pub environment: Environment,
    /// Nonsecret override for the image's default command; empty keeps it.
    pub command: Vec<String>,
}

/// A managed container observation. Environment values are never read back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Sandbox {
    pub container_id: String,
    pub sandbox_id: String,
    /// Docker's state, such as `running`, `exited`, or `created`.
    pub state: String,
    /// Hard CPU ceiling from NanoCPUs and CFS quota/period, rounded up to whole
    /// millicores. `None` means no finite quota: never account it as zero usage.
    /// CPU affinity and parent cgroups can further restrict actual execution.
    pub cpu_millicores: Option<u64>,
    /// Hard memory ceiling in bytes. `None` means unlimited, not zero usage.
    pub memory_bytes: Option<u64>,
}

/// Engine totals, without inferred free capacity or sandbox-slot policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineCapacity {
    pub cpu_millicores: u64,
    pub memory_bytes: u64,
}
