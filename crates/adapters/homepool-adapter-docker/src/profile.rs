//! Typed owner-selected settings and their Docker argument encoding.
use crate::{error::Error, settings::MAX_VOLUME_SUFFIX_BYTES};

/// A bind mount. The source belongs to the engine's machine, even over SSH.
#[derive(Clone, Debug, serde::Serialize)]
pub struct BindMount {
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct LaunchProfile {
    pub privileged: bool,
    pub pids_limit: Option<u64>,
    pub shm_bytes: Option<u64>,
    pub memory_swap_bytes: Option<u64>,
    pub tmpfs: Vec<TmpfsMount>,
    pub volumes: Vec<VolumeMount>,
    pub binds: Vec<BindMount>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct TmpfsMount {
    pub target: String,
    pub options: String,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct VolumeMount {
    pub name: String,
    pub target: String,
    pub read_only: bool,
}

/// Names are nonempty ASCII identifiers, also safe as Docker volume suffixes.
pub fn valid_name(name: &str) -> bool {
    name.bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

pub(crate) fn valid_mount_path(path: &str) -> bool {
    path.starts_with('/') && !path.contains([',', '"']) && !path.chars().any(char::is_control)
}

impl LaunchProfile {
    /// Validate without contacting Docker, including unselected member profiles.
    pub fn validate(&self) -> Result<(), Error> {
        if [self.pids_limit, self.shm_bytes, self.memory_swap_bytes]
            .into_iter()
            .flatten()
            .any(|v| v == 0 || v > i64::MAX as u64)
        {
            return Err(Error::InvalidInput("profile limits"));
        }
        if self
            .binds
            .iter()
            .any(|m| !valid_mount_path(&m.source) || !valid_mount_path(&m.target))
        {
            return Err(Error::InvalidInput("bind mount"));
        }
        if self.volumes.iter().any(|m| {
            !valid_name(&m.name)
                || m.name.len() > MAX_VOLUME_SUFFIX_BYTES
                || !valid_mount_path(&m.target)
        }) {
            return Err(Error::InvalidInput("volume mount"));
        }
        if self.tmpfs.iter().any(|m| {
            !valid_mount_path(&m.target)
                || m.target.contains(':')
                || !m
                    .options
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b",=._-".contains(&b))
        }) {
            return Err(Error::InvalidInput("tmpfs mount"));
        }
        Ok(())
    }

    pub(crate) fn append_args(&self, container: &str, args: &mut Vec<String>) -> Result<(), Error> {
        self.validate()?;
        if self.privileged {
            args.push("--privileged".into());
        }
        for (flag, value) in [
            ("--pids-limit", self.pids_limit),
            ("--shm-size", self.shm_bytes),
            ("--memory-swap", self.memory_swap_bytes),
        ] {
            if let Some(value) = value {
                args.extend([flag.into(), value.to_string()]);
            }
        }
        for mount in &self.tmpfs {
            args.extend([
                "--tmpfs".into(),
                format!("{}:{}", mount.target, mount.options),
            ]);
        }
        for mount in &self.volumes {
            let read_only = if mount.read_only { ",readonly" } else { "" };
            args.extend([
                "--mount".into(),
                format!(
                    "type=volume,src={container}-{},dst={}{read_only}",
                    mount.name, mount.target
                ),
            ]);
        }
        Ok(())
    }
}
