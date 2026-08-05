use std::path::PathBuf;
use std::time::Duration;

use dogi_core::{DogiError, Result};
use serde::{Deserialize, Serialize};

use crate::desktop::{UserContext, context as desktop};

const CONTROL_DIRECTORY: &str = "dogi";
const CONTROL_SOCKET: &str = "runtime-control.sock";
const PREVIEW_LEASE_TTL: Duration = Duration::from_secs(8);
const CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(6);

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
    pub(crate) fn for_desktop_user() -> Result<Self> {
        let context = desktop::current_user()?;
        Ok(Self {
            socket_path: control_socket_path(&context)?,
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
                    dogi_core::MIN_THUMB_WHEEL_SPEED_PERCENT,
                    dogi_core::MAX_THUMB_WHEEL_SPEED_PERCENT,
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
}

#[derive(Clone)]
pub(crate) struct RuntimePreviewState {
    #[cfg(unix)]
    shared: std::sync::Arc<SharedPreviewState>,
}

impl RuntimePreviewState {
    pub(crate) fn start() -> Result<Self> {
        #[cfg(unix)]
        {
            unix::start_server()
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
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum RuntimeControlRequest {
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
}

impl RuntimeControlResponse {
    fn success() -> Self {
        Self {
            ok: true,
            detail: String::new(),
        }
    }

    fn failure(detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            detail: detail.into(),
        }
    }

    fn into_result(self) -> Result<()> {
        if self.ok {
            Ok(())
        } else {
            Err(DogiError::BackendUnavailable(self.detail))
        }
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct PreviewLease {
    preview: HorizontalScrollPreview,
    expires_at: std::time::Instant,
}

#[cfg(unix)]
#[derive(Debug, Default)]
struct PreviewState {
    generation: u64,
    requested: Option<PreviewLease>,
    completed_generation: u64,
    completion: Option<std::result::Result<(), String>>,
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
}

#[cfg(unix)]
fn send_request(
    path: &std::path::Path,
    request: &RuntimeControlRequest,
) -> Result<RuntimeControlResponse> {
    use std::io::{BufRead, BufReader, Write};
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
    serde_json::to_writer(&mut stream, request).map_err(|error| {
        DogiError::Protocol(format!("failed to encode preview request: {error}"))
    })?;
    stream.write_all(b"\n").map_err(|error| {
        DogiError::Transport(format!("failed to send preview request: {error}"))
    })?;
    stream.flush().map_err(|error| {
        DogiError::Transport(format!("failed to flush preview request: {error}"))
    })?;

    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .map_err(|error| {
            DogiError::Transport(format!("failed to read preview response: {error}"))
        })?;
    serde_json::from_str(&response)
        .map_err(|error| DogiError::Protocol(format!("failed to decode preview response: {error}")))
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

fn control_socket_path(context: &UserContext) -> Result<PathBuf> {
    Ok(runtime_directory(context)?
        .join(CONTROL_DIRECTORY)
        .join(CONTROL_SOCKET))
}

fn runtime_directory(context: &UserContext) -> Result<PathBuf> {
    if let Some(uid) = context.uid {
        return Ok(PathBuf::from(format!("/run/user/{uid}")));
    }
    if let Some(path) = desktop::env_path("XDG_RUNTIME_DIR") {
        return Ok(path);
    }
    #[cfg(unix)]
    {
        Ok(PathBuf::from(format!("/run/user/{}", unsafe {
            libc::geteuid()
        })))
    }
    #[cfg(not(unix))]
    {
        Err(DogiError::BackendUnavailable(
            "XDG_RUNTIME_DIR is not set for the desktop session".to_owned(),
        ))
    }
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
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::Arc;

    use super::*;

    pub(super) fn start_server() -> Result<RuntimePreviewState> {
        let context = desktop::current_user()?;
        let path = control_socket_path(&context)?;
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
        std::thread::Builder::new()
            .name("dogi-runtime-control".to_owned())
            .spawn(move || {
                for connection in listener.incoming() {
                    match connection {
                        Ok(stream) => handle_connection(stream, &server_state),
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

    fn handle_connection(mut stream: UnixStream, shared: &SharedPreviewState) {
        let response = read_request(&stream)
            .map(|request| apply_request(shared, request))
            .unwrap_or_else(RuntimeControlResponse::failure);
        if let Err(error) = serde_json::to_writer(&mut stream, &response)
            .and_then(|_| stream.write_all(b"\n").map_err(serde_json::Error::io))
        {
            eprintln!("Dogi runtime control response failed: {error}");
        }
    }

    fn read_request(stream: &UnixStream) -> std::result::Result<RuntimeControlRequest, String> {
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .map_err(|error| format!("failed to read runtime control request: {error}"))?;
        serde_json::from_str(&line)
            .map_err(|error| format!("invalid runtime control request: {error}"))
    }

    fn apply_request(
        shared: &SharedPreviewState,
        request: RuntimeControlRequest,
    ) -> RuntimeControlResponse {
        match request {
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
                        dogi_core::MIN_THUMB_WHEEL_SPEED_PERCENT,
                        dogi_core::MAX_THUMB_WHEEL_SPEED_PERCENT,
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
}
