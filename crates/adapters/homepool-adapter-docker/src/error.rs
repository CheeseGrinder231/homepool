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
    /// Creation succeeded but starting failed. Both causes are redacted.
    /// Failed cleanup leaves the final mutation outcome uncertain; reconcile
    /// this known container id before retrying or releasing its allocation.
    StartFailed {
        container_id: String,
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
            _ => None,
        }
    }
}
