mod journal;
mod redact;

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::environment::AppEnvironment;

pub(crate) const ENTRY_LIMIT: usize = 200;
const MESSAGE_BYTES: usize = 1024;
const RECORD_PREFIX: &str = "DOGI_LOG ";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Level {
    Error,
    Warning,
    Info,
    Debug,
}

impl Level {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warning => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
        }
    }

    pub(crate) fn matches(self, filter: i32) -> bool {
        match filter {
            1 => matches!(self, Self::Error | Self::Warning),
            2 => self == Self::Error,
            _ => true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Entry {
    pub(crate) timestamp_ms: u64,
    pub(crate) level: Level,
    pub(crate) message: String,
}

impl Entry {
    pub(crate) fn time(&self) -> String {
        i64::try_from(self.timestamp_ms / 1000)
            .ok()
            .and_then(|seconds| gio::glib::DateTime::from_unix_local(seconds).ok())
            .and_then(|date| date.format("%Y-%m-%d %H:%M:%S").ok())
            .map(|date| date.to_string())
            .unwrap_or_else(|| "—".to_owned())
    }

    pub(crate) fn text(&self) -> String {
        format!(
            "{}  {:5}  {}",
            self.time(),
            self.level.label(),
            self.message
        )
    }

    fn sanitized(mut self, home: &str) -> Self {
        self.message = redact::sanitize(&self.message, home);
        self
    }
}

#[derive(Default)]
struct Buffer(VecDeque<Entry>);

impl Buffer {
    fn push(&mut self, entry: Entry) {
        if self.0.len() == ENTRY_LIMIT {
            self.0.pop_front();
        }
        self.0.push_back(entry);
    }

    fn snapshot(&self) -> Vec<Entry> {
        self.0.iter().cloned().collect()
    }
}

struct Logger {
    home: String,
    buffer: Mutex<Buffer>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        // Third-party debug logs can contain URLs, headers and input events.
        metadata.target().starts_with("dogi::") && metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let entry = Entry {
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| {
                    duration.as_millis().min(u64::MAX as u128) as u64
                }),
            level: match record.level() {
                log::Level::Error => Level::Error,
                log::Level::Warn => Level::Warning,
                log::Level::Info => Level::Info,
                log::Level::Debug | log::Level::Trace => Level::Debug,
            },
            message: record.args().to_string(),
        }
        .sanitized(&self.home);
        self.buffer
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(entry.clone());
        // One structured line, so journald retains severity and cannot split an entry.
        if let Ok(json) = serde_json::to_string(&entry) {
            use std::io::Write;
            let _ = writeln!(std::io::stderr().lock(), "{RECORD_PREFIX}{json}");
        }
    }

    fn flush(&self) {}
}

pub(crate) fn initialize(environment: &AppEnvironment, debug_events: bool) {
    let logger = LOGGER.get_or_init(|| Logger {
        home: environment.user.home.to_string_lossy().into_owned(),
        buffer: Mutex::new(Buffer::default()),
    });
    if log::set_logger(logger).is_ok() {
        log::set_max_level(if debug_events {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        });
    }
}

pub(crate) fn recent() -> Vec<Entry> {
    LOGGER.get().map_or_else(Vec::new, |logger| {
        logger
            .buffer
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .snapshot()
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Source {
    Application,
    Background,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReadError {
    BackgroundUnavailable,
    JournalUnavailable,
    TimedOut,
    TooLarge,
    InvalidData,
}

#[derive(Clone, Default)]
pub(crate) struct Reader {
    environment: Option<AppEnvironment>,
}

impl Reader {
    pub(crate) fn new(environment: AppEnvironment) -> Self {
        Self {
            environment: Some(environment),
        }
    }

    pub(crate) fn read(&self, source: Source) -> Result<Vec<Entry>, ReadError> {
        if source == Source::Application {
            return Ok(recent());
        }
        let environment = self
            .environment
            .as_ref()
            .ok_or(ReadError::BackgroundUnavailable)?;
        let entries = if environment.is_development() {
            // Never read the installed service from a development build.
            crate::runtime::control::RuntimeControlClient::for_environment(environment)
                .and_then(|client| client.recent_logs())
                .map_err(|_| ReadError::BackgroundUnavailable)?
        } else {
            journal::read()?
        };
        let home = environment.user.home.to_string_lossy();
        let start = entries.len().saturating_sub(ENTRY_LIMIT);
        Ok(entries
            .into_iter()
            .skip(start)
            .map(|entry| entry.sanitized(&home))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_keeps_only_the_latest_entries_in_order() {
        let mut buffer = Buffer::default();
        for timestamp_ms in 0..(ENTRY_LIMIT as u64 + 10) {
            buffer.push(Entry {
                timestamp_ms,
                level: Level::Info,
                message: "test".into(),
            });
        }
        let entries = buffer.snapshot();
        assert_eq!(entries.len(), ENTRY_LIMIT);
        assert_eq!(entries[0].timestamp_ms, 10);
        assert_eq!(
            entries[ENTRY_LIMIT - 1].timestamp_ms,
            ENTRY_LIMIT as u64 + 9
        );
    }

    #[test]
    fn severity_filters_include_errors_in_warnings() {
        assert!(Level::Error.matches(1));
        assert!(Level::Warning.matches(1));
        assert!(!Level::Info.matches(1));
        assert!(!Level::Warning.matches(2));
        assert!(Level::Debug.matches(0));
    }

    #[test]
    fn absent_background_is_distinct_from_an_empty_log() {
        assert_eq!(
            Reader::default().read(Source::Background),
            Err(ReadError::BackgroundUnavailable)
        );
        assert!(Reader::default().read(Source::Application).is_ok());
    }
}
