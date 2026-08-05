use std::fmt;

pub type Result<T> = std::result::Result<T, DogiError>;

#[derive(Debug)]
pub enum DogiError {
    DeviceNotFound,
    InvalidArgument(String),
    BackendUnavailable(String),
    UnsupportedFeature(String),
    Transport(String),
    Protocol(String),
    Config(String),
    Ui(String),
}

impl fmt::Display for DogiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeviceNotFound => write!(f, "device not found"),
            Self::InvalidArgument(message) => write!(f, "invalid argument: {message}"),
            Self::BackendUnavailable(message) => write!(f, "backend unavailable: {message}"),
            Self::UnsupportedFeature(feature) => write!(f, "unsupported feature: {feature}"),
            Self::Transport(message) => write!(f, "transport error: {message}"),
            Self::Protocol(message) => write!(f, "protocol error: {message}"),
            Self::Config(message) => write!(f, "config error: {message}"),
            Self::Ui(message) => write!(f, "ui error: {message}"),
        }
    }
}

impl std::error::Error for DogiError {}
