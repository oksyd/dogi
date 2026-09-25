use std::path::PathBuf;
use std::time::Duration;

use crate::domain::{DogiError, Result};
use serde::{Deserialize, Serialize};

use crate::environment::{AppEnvironment, AppPaths};

const PREVIEW_LEASE_TTL: Duration = Duration::from_secs(8);
const CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(6);
#[cfg(unix)]
const CONTROL_IO_TIMEOUT: Duration = Duration::from_millis(750);
#[cfg(unix)]
const CONTROL_MAX_FRAME_BYTES: usize = 8 * 1024;
#[cfg(unix)]
const CONTROL_MAX_CONNECTIONS: usize = 8;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeReadiness {
    Starting,
    Ready,
    Paused,
    Reconnecting,
    Degraded,
    Terminal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RuntimeHealthSnapshot {
    pub(crate) version: String,
    pub(crate) readiness: RuntimeReadiness,
    pub(crate) degraded: bool,
    pub(crate) last_error: Option<String>,
}

impl RuntimeHealthSnapshot {
    fn new(readiness: RuntimeReadiness, last_error: Option<String>) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            readiness,
            degraded: matches!(
                readiness,
                RuntimeReadiness::Reconnecting
                    | RuntimeReadiness::Degraded
                    | RuntimeReadiness::Terminal
            ),
            last_error,
        }
    }

    pub(crate) fn ready(&self) -> bool {
        self.readiness == RuntimeReadiness::Ready && !self.degraded
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HorizontalScrollPreview {
    pub(crate) lease_id: String,
    pub(crate) device_id: String,
    pub(crate) speed_percent: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreviewSnapshot {
    pub(crate) generation: u64,
    pub(crate) preview: Option<HorizontalScrollPreview>,
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeControlClient {
    socket_path: PathBuf,
    lease_id: String,
}

impl RuntimeControlClient {
    pub(crate) fn for_environment(environment: &AppEnvironment) -> Result<Self> {
        Ok(Self {
            socket_path: environment.paths.runtime_control_socket(),
            lease_id: format!("gui-{}-{}", std::process::id(), monotonic_nonce()),
        })
    }

    pub(crate) fn set_horizontal_scroll_preview(
        &self,
        device_id: &str,
        speed_percent: u16,
    ) -> Result<()> {
        let device_id = device_id.trim();
        if device_id.is_empty() {
            return Err(DogiError::InvalidArgument(
                "horizontal scroll preview requires a device id".to_owned(),
            ));
        }
        let response = send_request(
            &self.socket_path,
            &RuntimeControlRequest::SetHorizontalScrollPreview {
                lease_id: self.lease_id.clone(),
                device_id: device_id.to_owned(),
                speed_percent: speed_percent.clamp(
                    crate::domain::MIN_THUMB_WHEEL_SPEED_PERCENT,
                    crate::domain::MAX_THUMB_WHEEL_SPEED_PERCENT,
                ),
            },
        )?;
        response.into_result()
    }

    pub(crate) fn clear_horizontal_scroll_preview(&self) -> Result<()> {
        let response = send_request(
            &self.socket_path,
            &RuntimeControlRequest::ClearHorizontalScrollPreview {
                lease_id: self.lease_id.clone(),
            },
        )?;
        response.into_result()
    }

    pub(crate) fn status(&self) -> Result<RuntimeHealthSnapshot> {
        send_request(&self.socket_path, &RuntimeControlRequest::Status)?.into_status()
    }
}

#[derive(Clone)]
pub(crate) struct RuntimePreviewState {
    #[cfg(unix)]
    shared: std::sync::Arc<SharedPreviewState>,
}

impl RuntimePreviewState {
    pub(crate) fn start(paths: &AppPaths) -> Result<Self> {
        #[cfg(unix)]
        {
            unix::start_server(paths)
        }
        #[cfg(not(unix))]
        {
            Err(DogiError::BackendUnavailable(
                "runtime preview control requires Unix sockets".to_owned(),
            ))
        }
    }

    pub(crate) fn snapshot(&self) -> PreviewSnapshot {
        #[cfg(unix)]
        {
            self.shared.snapshot()
        }
        #[cfg(not(unix))]
        {
            PreviewSnapshot {
                generation: 0,
                preview: None,
            }
        }
    }

    pub(crate) fn publish_applied(&self, generation: u64) {
        #[cfg(unix)]
        self.shared.publish(generation, Ok(()));
    }

    pub(crate) fn publish_failed(&self, generation: u64, detail: impl Into<String>) {
        #[cfg(unix)]
        self.shared.publish(generation, Err(detail.into()));
    }

    pub(crate) fn fail_pending(&self, detail: impl Into<String>) {
        #[cfg(unix)]
        self.shared.fail_pending(detail.into());
    }

    pub(crate) fn publish_health(&self, readiness: RuntimeReadiness, last_error: Option<String>) {
        #[cfg(unix)]
        self.shared
            .publish_health(RuntimeHealthSnapshot::new(readiness, last_error));
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum RuntimeControlRequest {
    Status,
    SetHorizontalScrollPreview {
        lease_id: String,
        device_id: String,
        speed_percent: u16,
    },
    ClearHorizontalScrollPreview {
        lease_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RuntimeControlResponse {
    ok: bool,
    detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<RuntimeHealthSnapshot>,
}

impl RuntimeControlResponse {
    fn success() -> Self {
        Self {
            ok: true,
            detail: String::new(),
            status: None,
        }
    }

    fn failure(detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            detail: detail.into(),
            status: None,
        }
    }

    fn status(status: RuntimeHealthSnapshot) -> Self {
        Self {
            ok: true,
            detail: String::new(),
            status: Some(status),
        }
    }

    fn into_result(self) -> Result<()> {
        if self.ok {
            Ok(())
        } else {
            Err(DogiError::BackendUnavailable(self.detail))
        }
    }

    fn into_status(self) -> Result<RuntimeHealthSnapshot> {
        if !self.ok {
            return Err(DogiError::BackendUnavailable(self.detail));
        }
        self.status.ok_or_else(|| {
            DogiError::Protocol("runtime status response did not include health data".to_owned())
        })
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct PreviewLease {
    preview: HorizontalScrollPreview,
    expires_at: std::time::Instant,
}

#[cfg(unix)]
#[derive(Debug)]
struct PreviewState {
    generation: u64,
    requested: Option<PreviewLease>,
    completed_generation: u64,
    completion: Option<std::result::Result<(), String>>,
    health: RuntimeHealthSnapshot,
}

#[cfg(unix)]
impl Default for PreviewState {
    fn default() -> Self {
        Self {
            generation: 0,
            requested: None,
            completed_generation: 0,
            completion: None,
            health: RuntimeHealthSnapshot::new(RuntimeReadiness::Starting, None),
        }
    }
}

#[cfg(unix)]
#[derive(Debug, Default)]
struct SharedPreviewState {
    state: std::sync::Mutex<PreviewState>,
    changed: std::sync::Condvar,
}

#[cfg(unix)]
impl SharedPreviewState {
    fn set_preview(
        &self,
        lease_id: String,
        device_id: String,
        speed_percent: u16,
    ) -> std::result::Result<(u64, bool), String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let next = HorizontalScrollPreview {
            lease_id,
            device_id,
            speed_percent,
        };
        if state
            .requested
            .as_ref()
            .is_some_and(|lease| lease.preview.lease_id != next.lease_id)
        {
            return Err("another Dogi window is already testing horizontal scrolling".to_owned());
        }
        let unchanged = state
            .requested
            .as_ref()
            .is_some_and(|lease| lease.preview == next);

        if unchanged {
            if let Some(lease) = state.requested.as_mut() {
                lease.expires_at = std::time::Instant::now() + PREVIEW_LEASE_TTL;
            }
            return Ok((
                state.generation,
                state.completed_generation == state.generation
                    && state
                        .completion
                        .as_ref()
                        .is_some_and(|result| result.is_ok()),
            ));
        }

        state.generation = state.generation.wrapping_add(1).max(1);
        state.requested = Some(PreviewLease {
            preview: next,
            expires_at: std::time::Instant::now() + PREVIEW_LEASE_TTL,
        });
        state.completion = None;
        Ok((state.generation, false))
    }

    fn clear_preview(&self, lease_id: &str) -> (u64, bool) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let owns_lease = state
            .requested
            .as_ref()
            .is_some_and(|lease| lease.preview.lease_id == lease_id);
        if !owns_lease {
            return (state.generation, true);
        }

        state.generation = state.generation.wrapping_add(1).max(1);
        state.requested = None;
        state.completion = None;
        (state.generation, false)
    }

    fn snapshot(&self) -> PreviewSnapshot {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let expired = state
            .requested
            .as_ref()
            .is_some_and(|lease| lease.expires_at <= std::time::Instant::now());
        if expired {
            state.generation = state.generation.wrapping_add(1).max(1);
            state.requested = None;
            state.completion = None;
        }

        PreviewSnapshot {
            generation: state.generation,
            preview: state.requested.as_ref().map(|lease| lease.preview.clone()),
        }
    }

    fn publish(&self, generation: u64, result: std::result::Result<(), String>) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.generation != generation {
            return;
        }
        state.completed_generation = generation;
        state.completion = Some(result);
        self.changed.notify_all();
    }

    fn fail_pending(&self, detail: String) {
        let generation = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .generation;
        self.publish(generation, Err(detail));
    }

    fn wait_for(&self, generation: u64) -> RuntimeControlResponse {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let (state, timeout) = self
            .changed
            .wait_timeout_while(state, CONTROL_RESPONSE_TIMEOUT, |state| {
                state.generation == generation && state.completed_generation != generation
            })
            .unwrap_or_else(|error| error.into_inner());

        if state.generation != generation {
            return RuntimeControlResponse::failure(
                "horizontal scroll preview was replaced by another request",
            );
        }
        if timeout.timed_out() && state.completed_generation != generation {
            return RuntimeControlResponse::failure(
                "the Dogi runtime did not activate the preview in time",
            );
        }
        match state.completion.as_ref() {
            Some(Ok(())) => RuntimeControlResponse::success(),
            Some(Err(detail)) => RuntimeControlResponse::failure(detail.clone()),
            None => {
                RuntimeControlResponse::failure("the Dogi runtime did not report the preview state")
            }
        }
    }

    fn publish_health(&self, health: RuntimeHealthSnapshot) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .health = health;
    }

    fn health(&self) -> RuntimeHealthSnapshot {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .health
            .clone()
    }
}

#[cfg(unix)]
fn send_request(
    path: &std::path::Path,
    request: &RuntimeControlRequest,
) -> Result<RuntimeControlResponse> {
    use std::io::{BufReader, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(path).map_err(|error| {
        DogiError::BackendUnavailable(format!(
            "Dogi runtime control is unavailable at {}: {error}",
            path.display()
        ))
    })?;
    stream
        .set_read_timeout(Some(CONTROL_RESPONSE_TIMEOUT + Duration::from_secs(1)))
        .map_err(|error| {
            DogiError::Transport(format!("failed to configure preview control: {error}"))
        })?;
    stream
        .set_write_timeout(Some(CONTROL_IO_TIMEOUT))
        .map_err(|error| {
            DogiError::Transport(format!("failed to configure preview control: {error}"))
        })?;
    serde_json::to_writer(&mut stream, request).map_err(|error| {
        DogiError::Protocol(format!("failed to encode preview request: {error}"))
    })?;
    stream.write_all(b"\n").map_err(|error| {
        DogiError::Transport(format!("failed to send preview request: {error}"))
    })?;
    stream.flush().map_err(|error| {
        DogiError::Transport(format!("failed to flush preview request: {error}"))
    })?;

    let response = read_control_frame(&mut BufReader::new(stream)).map_err(|error| {
        DogiError::Transport(format!("failed to read preview response: {error}"))
    })?;
    serde_json::from_slice(&response)
        .map_err(|error| DogiError::Protocol(format!("failed to decode preview response: {error}")))
}

#[cfg(unix)]
fn read_control_frame(reader: &mut impl std::io::BufRead) -> std::result::Result<Vec<u8>, String> {
    let mut frame = Vec::with_capacity(256);
    let mut limited = std::io::Read::take(&mut *reader, (CONTROL_MAX_FRAME_BYTES + 1) as u64);
    std::io::BufRead::read_until(&mut limited, b'\n', &mut frame)
        .map_err(|error| format!("control frame read failed: {error}"))?;
    if frame.len() > CONTROL_MAX_FRAME_BYTES {
        return Err(format!(
            "control frame exceeds the {CONTROL_MAX_FRAME_BYTES}-byte limit"
        ));
    }
    if frame.is_empty() {
        return Err("control connection closed before a frame was received".to_owned());
    }
    if frame.last() != Some(&b'\n') {
        return Err("control frame is not newline-terminated".to_owned());
    }
    Ok(frame)
}

#[cfg(not(unix))]
fn send_request(
    _path: &std::path::Path,
    _request: &RuntimeControlRequest,
) -> Result<RuntimeControlResponse> {
    Err(DogiError::BackendUnavailable(
        "runtime preview control requires Unix sockets".to_owned(),
    ))
}

fn monotonic_nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::io::{BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    pub(super) fn start_server(paths: &AppPaths) -> Result<RuntimePreviewState> {
        let path = paths.runtime_control_socket();
        let directory = path.parent().ok_or_else(|| {
            DogiError::Config(format!(
                "runtime control path has no parent: {}",
                path.display()
            ))
        })?;
        fs::create_dir_all(directory).map_err(|error| {
            DogiError::Config(format!(
                "failed to create runtime control directory {}: {error}",
                directory.display()
            ))
        })?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).map_err(|error| {
            DogiError::Config(format!(
                "failed to secure runtime control directory {}: {error}",
                directory.display()
            ))
        })?;
        if path.exists() {
            if UnixStream::connect(&path).is_ok() {
                return Err(DogiError::BackendUnavailable(format!(
                    "another Dogi runtime already owns {}",
                    path.display()
                )));
            }
            fs::remove_file(&path).map_err(|error| {
                DogiError::Config(format!(
                    "failed to replace stale runtime control socket {}: {error}",
                    path.display()
                ))
            })?;
        }

        let listener = UnixListener::bind(&path).map_err(|error| {
            DogiError::BackendUnavailable(format!(
                "failed to bind runtime control socket {}: {error}",
                path.display()
            ))
        })?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|error| {
            DogiError::Config(format!(
                "failed to secure runtime control socket {}: {error}",
                path.display()
            ))
        })?;

        let shared = Arc::new(SharedPreviewState::default());
        let server_state = shared.clone();
        let connections = ConnectionLimiter::new();
        std::thread::Builder::new()
            .name("dogi-runtime-control".to_owned())
            .spawn(move || {
                for connection in listener.incoming() {
                    match connection {
                        Ok(stream) => connections.dispatch(stream, server_state.clone()),
                        Err(error) => eprintln!("Dogi runtime control connection failed: {error}"),
                    }
                }
            })
            .map_err(|error| {
                DogiError::BackendUnavailable(format!(
                    "failed to start runtime control server: {error}"
                ))
            })?;

        Ok(RuntimePreviewState { shared })
    }

    pub(super) struct ConnectionLimiter {
        active: Arc<AtomicUsize>,
    }

    impl ConnectionLimiter {
        pub(super) fn new() -> Self {
            Self {
                active: Arc::new(AtomicUsize::new(0)),
            }
        }

        pub(super) fn dispatch(&self, stream: UnixStream, shared: Arc<SharedPreviewState>) {
            let Some(permit) = ConnectionPermit::try_acquire(self.active.clone()) else {
                reject_connection(stream, "runtime control is busy; try again shortly");
                return;
            };
            if let Err(error) = std::thread::Builder::new()
                .name("dogi-runtime-control-client".to_owned())
                .spawn(move || {
                    let _permit = permit;
                    handle_connection(stream, &shared);
                })
            {
                eprintln!("Dogi runtime control client could not be started: {error}");
            }
        }
    }

    struct ConnectionPermit {
        active: Arc<AtomicUsize>,
    }

    impl ConnectionPermit {
        fn try_acquire(active: Arc<AtomicUsize>) -> Option<Self> {
            active
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    (current < CONTROL_MAX_CONNECTIONS).then_some(current + 1)
                })
                .ok()
                .map(|_| Self { active })
        }
    }

    impl Drop for ConnectionPermit {
        fn drop(&mut self) {
            self.active.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn handle_connection(mut stream: UnixStream, shared: &SharedPreviewState) {
        if let Err(error) = configure_connection(&stream) {
            eprintln!("Dogi runtime control connection could not be configured: {error}");
            return;
        }
        let response = read_request(&stream)
            .map(|request| apply_request(shared, request))
            .unwrap_or_else(RuntimeControlResponse::failure);
        if let Err(error) = serde_json::to_writer(&mut stream, &response)
            .and_then(|_| stream.write_all(b"\n").map_err(serde_json::Error::io))
        {
            eprintln!("Dogi runtime control response failed: {error}");
        }
    }

    fn reject_connection(mut stream: UnixStream, detail: &str) {
        if configure_connection(&stream).is_err() {
            return;
        }
        let response = RuntimeControlResponse::failure(detail);
        let _ = serde_json::to_writer(&mut stream, &response)
            .and_then(|_| stream.write_all(b"\n").map_err(serde_json::Error::io));
    }

    fn configure_connection(stream: &UnixStream) -> std::io::Result<()> {
        stream.set_read_timeout(Some(CONTROL_IO_TIMEOUT))?;
        stream.set_write_timeout(Some(CONTROL_IO_TIMEOUT))
    }

    fn read_request(stream: &UnixStream) -> std::result::Result<RuntimeControlRequest, String> {
        let frame = read_control_frame(&mut BufReader::new(stream))?;
        serde_json::from_slice(&frame)
            .map_err(|error| format!("invalid runtime control request: {error}"))
    }

    pub(super) fn apply_request(
        shared: &SharedPreviewState,
        request: RuntimeControlRequest,
    ) -> RuntimeControlResponse {
        match request {
            RuntimeControlRequest::Status => RuntimeControlResponse::status(shared.health()),
            RuntimeControlRequest::SetHorizontalScrollPreview {
                lease_id,
                device_id,
                speed_percent,
            } => {
                if lease_id.trim().is_empty() || device_id.trim().is_empty() {
                    return RuntimeControlResponse::failure(
                        "horizontal scroll preview identifiers cannot be empty",
                    );
                }
                let request_lease = lease_id.clone();
                let (generation, already_applied) = match shared.set_preview(
                    lease_id,
                    device_id,
                    speed_percent.clamp(
                        crate::domain::MIN_THUMB_WHEEL_SPEED_PERCENT,
                        crate::domain::MAX_THUMB_WHEEL_SPEED_PERCENT,
                    ),
                ) {
                    Ok(result) => result,
                    Err(detail) => return RuntimeControlResponse::failure(detail),
                };
                let response = if already_applied {
                    RuntimeControlResponse::success()
                } else {
                    shared.wait_for(generation)
                };
                if !response.ok {
                    let _ = shared.clear_preview(&request_lease);
                }
                response
            }
            RuntimeControlRequest::ClearHorizontalScrollPreview { lease_id } => {
                let (generation, already_applied) = shared.clear_preview(&lease_id);
                if already_applied {
                    RuntimeControlResponse::success()
                } else {
                    shared.wait_for(generation)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn preview_lease_refresh_does_not_create_a_new_generation() {
        let shared = SharedPreviewState::default();
        let (generation, ready) = shared
            .set_preview("lease".to_owned(), "device".to_owned(), 175)
            .unwrap();
        assert!(!ready);
        shared.publish(generation, Ok(()));

        let (refreshed_generation, ready) = shared
            .set_preview("lease".to_owned(), "device".to_owned(), 175)
            .unwrap();

        assert_eq!(refreshed_generation, generation);
        assert!(ready);
    }

    #[cfg(unix)]
    #[test]
    fn stale_owner_cannot_clear_a_newer_preview() {
        let shared = SharedPreviewState::default();
        shared
            .set_preview("new".to_owned(), "device".to_owned(), 200)
            .unwrap();

        let (_, already_applied) = shared.clear_preview("old");

        assert!(already_applied);
        assert_eq!(
            shared.snapshot().preview.unwrap().lease_id,
            "new".to_owned()
        );
    }

    #[test]
    fn response_failure_maps_to_backend_error() {
        let error = RuntimeControlResponse::failure("preview unavailable")
            .into_result()
            .unwrap_err();
        assert!(error.to_string().contains("preview unavailable"));
    }

    #[cfg(unix)]
    #[test]
    fn health_status_reports_version_readiness_and_last_error() {
        let shared = SharedPreviewState::default();
        shared.publish_health(RuntimeHealthSnapshot::new(
            RuntimeReadiness::Degraded,
            Some("device unavailable".to_owned()),
        ));

        let response = unix::apply_request(&shared, RuntimeControlRequest::Status);
        let status = response.into_status().unwrap();

        assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(status.readiness, RuntimeReadiness::Degraded);
        assert!(status.degraded);
        assert_eq!(status.last_error.as_deref(), Some("device unavailable"));
    }

    #[cfg(unix)]
    #[test]
    fn active_preview_lease_cannot_be_stolen_by_another_gui() {
        let shared = SharedPreviewState::default();
        shared
            .set_preview("first".to_owned(), "device".to_owned(), 125)
            .unwrap();

        let error = shared
            .set_preview("second".to_owned(), "device".to_owned(), 200)
            .unwrap_err();

        assert!(error.contains("another Dogi window"));
        assert_eq!(shared.snapshot().preview.unwrap().lease_id, "first");
    }

    #[cfg(unix)]
    #[test]
    fn idle_control_client_does_not_block_a_status_query() {
        use std::io::{BufReader, Write};
        use std::os::unix::net::UnixStream;
        use std::sync::Arc;

        let shared = Arc::new(SharedPreviewState::default());
        let connections = unix::ConnectionLimiter::new();
        let (idle_client, idle_server) = UnixStream::pair().unwrap();
        connections.dispatch(idle_server, shared.clone());

        let (mut status_client, status_server) = UnixStream::pair().unwrap();
        status_client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        connections.dispatch(status_server, shared);
        serde_json::to_writer(&mut status_client, &RuntimeControlRequest::Status).unwrap();
        status_client.write_all(b"\n").unwrap();
        let frame = read_control_frame(&mut BufReader::new(status_client)).unwrap();
        let response = serde_json::from_slice::<RuntimeControlResponse>(&frame).unwrap();

        assert!(response.into_status().is_ok());
        drop(idle_client);
    }

    #[cfg(unix)]
    #[test]
    fn oversized_control_frame_is_rejected() {
        use std::io::{BufReader, Write};
        use std::os::unix::net::UnixStream;
        use std::sync::Arc;

        let shared = Arc::new(SharedPreviewState::default());
        let connections = unix::ConnectionLimiter::new();
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        connections.dispatch(server, shared);
        client
            .write_all(&vec![b'x'; CONTROL_MAX_FRAME_BYTES + 1])
            .unwrap();
        client.write_all(b"\n").unwrap();
        let frame = read_control_frame(&mut BufReader::new(client)).unwrap();
        let response = serde_json::from_slice::<RuntimeControlResponse>(&frame).unwrap();

        assert!(!response.ok);
        assert!(response.detail.contains("exceeds"));
    }

    #[cfg(unix)]
    #[test]
    fn control_connection_limit_rejects_excess_clients() {
        use std::io::BufReader;
        use std::os::unix::net::UnixStream;
        use std::sync::Arc;

        let shared = Arc::new(SharedPreviewState::default());
        let connections = unix::ConnectionLimiter::new();
        let mut idle_clients = Vec::new();
        for _ in 0..CONTROL_MAX_CONNECTIONS {
            let (client, server) = UnixStream::pair().unwrap();
            connections.dispatch(server, shared.clone());
            idle_clients.push(client);
        }

        let (excess_client, excess_server) = UnixStream::pair().unwrap();
        excess_client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        connections.dispatch(excess_server, shared);
        let frame = read_control_frame(&mut BufReader::new(excess_client)).unwrap();
        let response = serde_json::from_slice::<RuntimeControlResponse>(&frame).unwrap();

        assert!(!response.ok);
        assert!(response.detail.contains("busy"));
        drop(idle_clients);
    }
}
