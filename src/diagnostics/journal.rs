use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{ENTRY_LIMIT, Entry, Level, RECORD_PREFIX, ReadError};

const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;

pub(super) fn read() -> Result<Vec<Entry>, ReadError> {
    let output = bounded_output(
        Command::new("journalctl").args([
            "--user",
            "--unit=dogi-runtime.service",
            "--boot",
            "--lines=200",
            "--output=json",
            "--output-fields=__REALTIME_TIMESTAMP,PRIORITY,MESSAGE",
            "--no-pager",
            "--quiet",
        ]),
        Duration::from_secs(3),
        OUTPUT_LIMIT,
    )?;
    parse(&output)
}

fn parse(output: &[u8]) -> Result<Vec<Entry>, ReadError> {
    let mut entries = Vec::new();
    for line in output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: serde_json::Value =
            serde_json::from_slice(line).map_err(|_| ReadError::InvalidData)?;
        let Some(message) = record["MESSAGE"].as_str() else {
            continue;
        };
        if let Some(json) = message.strip_prefix(RECORD_PREFIX) {
            entries.push(serde_json::from_str(json).map_err(|_| ReadError::InvalidData)?);
        } else {
            let message = message.trim();
            // Older builds emitted every input event and active window title.
            if [
                "event:",
                "action:",
                "execution:",
                "active app:",
                "active profile:",
            ]
            .iter()
            .any(|prefix| message.starts_with(prefix))
            {
                continue;
            }
            let level = match record["PRIORITY"].as_str() {
                Some("0" | "1" | "2" | "3") => Level::Error,
                Some("4") => Level::Warning,
                Some("7") => Level::Debug,
                _ => Level::Info,
            };
            entries.push(Entry {
                timestamp_ms: record["__REALTIME_TIMESTAMP"]
                    .as_str()
                    .and_then(|timestamp| timestamp.parse::<u64>().ok())
                    .unwrap_or_default()
                    / 1000,
                level,
                message: message.to_owned(),
            });
        }
    }
    if entries.len() > ENTRY_LIMIT {
        entries.drain(..entries.len() - ENTRY_LIMIT);
    }
    Ok(entries)
}

#[cfg(unix)]
fn bounded_output(
    command: &mut Command,
    timeout: Duration,
    limit: usize,
) -> Result<Vec<u8>, ReadError> {
    use std::io::Read;
    use std::os::fd::AsRawFd;

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ReadError::JournalUnavailable)?;
    let result = (|| {
        let mut stdout = child.stdout.take().ok_or(ReadError::JournalUnavailable)?;
        let fd = stdout.as_raw_fd();
        // SAFETY: stdout owns this live descriptor for the duration of both calls.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        // SAFETY: F_SETFL only changes flags on the live pipe; no pointers are passed.
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(ReadError::JournalUnavailable);
        }
        let deadline = Instant::now() + timeout;
        let mut output = Vec::new();
        let mut chunk = [0; 8192];
        loop {
            if Instant::now() >= deadline {
                return Err(ReadError::TimedOut);
            }
            match stdout.read(&mut chunk) {
                Ok(0) => {
                    if let Some(status) = child
                        .try_wait()
                        .map_err(|_| ReadError::JournalUnavailable)?
                    {
                        return if status.success() {
                            Ok(output)
                        } else {
                            Err(ReadError::JournalUnavailable)
                        };
                    }
                }
                Ok(count) => {
                    if output.len() + count > limit {
                        return Err(ReadError::TooLarge);
                    }
                    output.extend_from_slice(&chunk[..count]);
                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(ReadError::JournalUnavailable),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

#[cfg(not(unix))]
fn bounded_output(_: &mut Command, _: Duration, _: usize) -> Result<Vec<u8>, ReadError> {
    Err(ReadError::JournalUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_preserves_structured_severity_and_skips_legacy_input() {
        let entry = Entry {
            timestamp_ms: 1234,
            level: Level::Warning,
            message: "Reconnecting".into(),
        };
        let structured = serde_json::json!({"MESSAGE": format!("{RECORD_PREFIX}{}", serde_json::to_string(&entry).unwrap()), "PRIORITY": "6"});
        let legacy =
            serde_json::json!({"MESSAGE": "  active app: private window title", "PRIORITY": "6"});
        assert_eq!(
            parse(format!("{structured}\n{legacy}\n").as_bytes()).unwrap(),
            [entry]
        );
        assert!(parse(b"").unwrap().is_empty());
        assert_eq!(parse(b"invalid"), Err(ReadError::InvalidData));
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_is_bounded_by_time_and_output_and_rejects_failure() {
        assert_eq!(
            bounded_output(
                Command::new("sleep").arg("2"),
                Duration::from_millis(30),
                64
            ),
            Err(ReadError::TimedOut)
        );
        assert_eq!(
            bounded_output(
                Command::new("printf").arg("123456789"),
                Duration::from_secs(1),
                4
            ),
            Err(ReadError::TooLarge)
        );
        assert_eq!(
            bounded_output(&mut Command::new("false"), Duration::from_secs(1), 64),
            Err(ReadError::JournalUnavailable)
        );
        assert_eq!(
            bounded_output(Command::new("printf").arg("ok"), Duration::from_secs(1), 64),
            Ok(b"ok".to_vec())
        );
    }
}
