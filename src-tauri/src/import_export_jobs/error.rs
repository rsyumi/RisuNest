use std::{fmt, io};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatErrorKind {
    Cancelled,
    InvalidFormat,
    LimitExceeded,
    Io,
}

#[derive(Debug)]
pub struct FormatError {
    pub kind: FormatErrorKind,
    pub message: String,
    source: Option<io::Error>,
}

impl FormatError {
    pub fn cancelled() -> Self {
        Self::new(FormatErrorKind::Cancelled, "import cancelled")
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(FormatErrorKind::InvalidFormat, message)
    }

    pub fn limit(message: impl Into<String>) -> Self {
        Self::new(FormatErrorKind::LimitExceeded, message)
    }

    pub fn io(operation: &str, source: io::Error) -> Self {
        Self {
            kind: FormatErrorKind::Io,
            message: format!("{operation}: {source}"),
            source: Some(source),
        }
    }

    fn new(kind: FormatErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }
}

impl fmt::Display for FormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FormatError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}
