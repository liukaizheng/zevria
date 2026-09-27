use rig_core::tool::ToolExecutionError;
use thiserror::Error;

/// Descriptive failure shared by the structured file-mutation tools.
#[derive(Debug, Error)]
pub enum FileToolError {
    #[error("{0}")]
    InvalidArguments(String),
    #[error("{0}")]
    PathResolution(String),
    #[error("{0}")]
    Io(String),
}

impl FileToolError {
    pub(crate) fn invalid_arguments(message: impl Into<String>) -> Self {
        Self::InvalidArguments(message.into())
    }

    pub(crate) fn path_resolution(message: impl Into<String>) -> Self {
        Self::PathResolution(message.into())
    }

    pub(crate) fn io(message: impl Into<String>) -> Self {
        Self::Io(message.into())
    }

    pub(crate) fn classify(&self) -> ToolExecutionError {
        match self {
            Self::InvalidArguments(_) | Self::PathResolution(_) => {
                ToolExecutionError::invalid_args(self.to_string())
            }
            Self::Io(_) => ToolExecutionError::other(self.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use rig_core::tool::ToolErrorKind;

    use super::*;

    #[test]
    fn classifies_argument_and_path_failures_as_invalid_args() {
        for error in [
            FileToolError::invalid_arguments("bad argument"),
            FileToolError::path_resolution("outside workspace"),
        ] {
            assert_eq!(error.classify().kind(), ToolErrorKind::InvalidArgs);
        }
        assert_eq!(
            FileToolError::io("disk failure").classify().kind(),
            ToolErrorKind::Other
        );
    }
}
