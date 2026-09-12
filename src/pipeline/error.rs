use std::fmt;

/// Errors that can occur during pipeline operations.
#[derive(Debug)]
pub enum PipelineError {
    /// I/O error.
    Io(std::io::Error),
    /// Channel/broadcast error.
    Channel(String),
    /// General pipeline error.
    Pipeline(String),
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipelineError::Io(err) => write!(f, "IO error: {}", err),
            PipelineError::Channel(msg) => write!(f, "Channel error: {}", msg),
            PipelineError::Pipeline(msg) => write!(f, "Pipeline error: {}", msg),
        }
    }
}

impl std::error::Error for PipelineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PipelineError::Io(err) => Some(err),
            PipelineError::Channel(_) | PipelineError::Pipeline(_) => None,
        }
    }
}

impl From<std::io::Error> for PipelineError {
    fn from(err: std::io::Error) -> Self {
        PipelineError::Io(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn test_display_io_error() {
        let err = PipelineError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file not found",
        ));
        assert!(err.to_string().contains("file not found"));
    }

    #[test]
    fn test_display_channel_error() {
        let err = PipelineError::Channel("buffer full".to_string());
        assert_eq!(err.to_string(), "Channel error: buffer full");
    }

    #[test]
    fn test_display_pipeline_error() {
        let err = PipelineError::Pipeline("general fault".to_string());
        assert_eq!(err.to_string(), "Pipeline error: general fault");
    }

    #[test]
    fn test_debug_format() {
        let err = PipelineError::Pipeline("test".to_string());
        assert!(format!("{err:?}").contains("Pipeline"));
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::other("disk failure");
        let err: PipelineError = io_err.into();
        match err {
            PipelineError::Io(_) => {}
            _ => panic!("Expected Io variant"),
        }
    }

    #[test]
    fn test_error_source() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no access");
        let err = PipelineError::Io(io_err);
        assert!(err.source().is_some());

        let chan_err = PipelineError::Channel("closed".to_string());
        assert!(chan_err.source().is_none());
    }
}
