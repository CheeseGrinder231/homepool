use std::fmt;

use crate::settings::CPU_LIMIT_QUANTUM_MILLICORES;

/// Redacted failures: never contain Docker's raw output or environment values.
#[derive(Debug)]
pub enum Error {
    InvalidInput(&'static str),
    /// Starts require CPU limits aligned to the runtime's accounting quantum.
    CpuLimitNotAligned {
        cpu_millicores: u64,
    },
    Io(&'static str),
    /// The local transport was terminated; remote mutation may still have happened.
    Deadline(&'static str),
    /// The local transport was cancelled; mutation outcome remains unknown.
    Cancelled,
    Command {
        operation: &'static str,
        code: Option<i32>,
    },
    Protocol(&'static str),
    NotManaged,
    /// The engine already owns the deterministic name for this identity.
    IdentityConflict {
        sandbox_id: String,
    },
    /// Start may have succeeded; final inspection failed. Reconcile this handle.
    StartUnconfirmed {
        container_id: String,
        sandbox_id: String,
        source: Box<Error>,
    },
    /// Creation succeeded but starting failed. Both causes are redacted.
    /// Failed cleanup leaves the final mutation outcome uncertain; reconcile
    /// this known container id before retrying or releasing its allocation.
    StartFailed {
        container_id: String,
        sandbox_id: String,
        start_error: Box<Error>,
        /// `None` means rollback confirmed removal of the container.
        cleanup_error: Option<Box<Error>>,
        /// True when rollback could not confirm removal, including on a deadline.
        outcome_uncertain: bool,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CpuLimitNotAligned { cpu_millicores } => write!(
                f,
                "CPU limit {cpu_millicores} millicores must be a multiple of {CPU_LIMIT_QUANTUM_MILLICORES}"
            ),
            Self::InvalidInput(field) => write!(f, "invalid {field}"),
            Self::Cancelled => f.write_str("Docker operation cancelled; reconcile before retrying"),
            Self::Deadline(operation) => write!(f, "Docker {operation} deadline exceeded"),
            Self::Io(operation) => write!(f, "Docker {operation} I/O failed"),
            Self::Command { operation, code } => write!(
                f,
                "Docker {operation} failed (exit {code:?}); diagnostics redacted"
            ),
            Self::Protocol(operation) => write!(f, "invalid Docker {operation} response"),
            Self::NotManaged => f.write_str("container is not managed by this member"),
            Self::IdentityConflict { .. } => {
                f.write_str("sandbox identity already exists on this member")
            }
            Self::StartUnconfirmed { source, .. } => write!(
                f,
                "Docker start outcome unconfirmed: {source}; reconcile before retrying"
            ),
            Self::StartFailed {
                container_id,
                sandbox_id: _,
                start_error,
                cleanup_error,
                outcome_uncertain,
            } => {
                write!(f, "Docker start failed for {container_id}: {start_error}; ")?;
                match cleanup_error {
                    Some(error) => write!(f, "cleanup failed: {error}")?,
                    None => f.write_str("cleanup succeeded")?,
                }
                write!(f, "; outcome uncertain: {outcome_uncertain}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::StartFailed { start_error, .. } => Some(start_error.as_ref()),
            Self::StartUnconfirmed { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl Error {
    /// A created container's known handle and caller identity, even on failure.
    pub fn known_sandbox(&self) -> Option<(&str, &str)> {
        match self {
            Self::StartFailed {
                container_id,
                sandbox_id,
                ..
            }
            | Self::StartUnconfirmed {
                container_id,
                sandbox_id,
                ..
            } => Some((container_id, sandbox_id)),
            _ => None,
        }
    }
}
