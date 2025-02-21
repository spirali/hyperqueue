use thiserror::Error;

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Error)]
pub enum DsError {
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    SerializationError(String),
    #[error("Scheduler error: {0}")]
    SchedulerError(String),
    #[error("{0}")]
    GenericError(String),
}

impl From<serde_json::error::Error> for DsError {
    fn from(e: serde_json::error::Error) -> Self {
        Self::SerializationError(e.to_string())
    }
}
impl From<bincode::Error> for DsError {
    fn from(e: bincode::Error) -> Self {
        Self::SerializationError(e.to_string())
    }
}
impl From<psutil::Error> for DsError {
    fn from(e: psutil::Error) -> Self {
        Self::GenericError(e.to_string())
    }
}
impl From<String> for DsError {
    fn from(e: String) -> Self {
        Self::GenericError(e)
    }
}
impl From<&str> for DsError {
    fn from(e: &str) -> Self {
        Self::GenericError(e.to_string())
    }
}
