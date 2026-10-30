use std::fmt;

/// Redacted failures: never contain Docker's raw output or environment values.
#[derive(Debug)]
pub enum Error {
    InvalidInput(&'static str),
    Io(&'static str),
    /// The local transport was terminated; remote mutation may still have happened.
    Deadline(&'static str),
    Command {
        operation: &'static str,
        code: Option<i32>,
    },
    Protocol(&'static str),
    NotManaged,
    /// Creation succeeded but starting failed. A failed cleanup leaves this id
    /// available for explicit recovery through `stop` and `remove`.
    StartFailed {
        container_id: String,
        cleanup_failed: bool,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(field) => write!(f, "invalid {field}"),
            Self::Deadline(operation) => write!(f, "Docker {operation} deadline exceeded"),
            Self::Io(operation) => write!(f, "Docker {operation} I/O failed"),
            Self::Command { operation, code } => write!(
                f,
                "Docker {operation} failed (exit {code:?}); diagnostics redacted"
            ),
            Self::Protocol(operation) => write!(f, "invalid Docker {operation} response"),
            Self::NotManaged => f.write_str("container is not managed by this member"),
            Self::StartFailed {
                container_id,
                cleanup_failed,
            } => write!(
                f,
                "Docker start failed for {container_id}; cleanup failed: {cleanup_failed}"
            ),
        }
    }
}

impl std::error::Error for Error {}
