use std::fmt;

/// Sanitized failures: never include subprocess output, request bodies or credentials.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error(pub(crate) &'static str);

impl Error {
    /// Cooperative cancellation, with any mutation's outcome still unknown.
    pub const CANCELLED: Self =
        Self("Garage operation cancelled; inspect privately before retrying");
    pub(crate) const DEADLINE: Self =
        Self("Garage operation deadline exceeded; inspect privately before retrying");
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Error {}
