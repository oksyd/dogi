use std::sync::{Arc, Mutex};
use std::time::Duration;

use zbus::MatchRule;
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::message::Type as MessageType;
use zbus::zvariant::OwnedObjectPath;

const LOGIN1_DESTINATION: &str = "org.freedesktop.login1";
const LOGIN1_MANAGER_PATH: &str = "/org/freedesktop/login1";
const LOGIN1_MANAGER_INTERFACE: &str = "org.freedesktop.login1.Manager";
const LOGIN1_SESSION_INTERFACE: &str = "org.freedesktop.login1.Session";
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

type LoginSession = (String, u32, String, String, OwnedObjectPath);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GraphicalSessionMode {
    LocalActive,
    LocalLocked,
    RemoteOnly,
    Inactive,
    Unknown,
}

impl GraphicalSessionMode {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::LocalActive => "local-active",
            Self::LocalLocked => "local-locked",
            Self::RemoteOnly => "remote-only",
            Self::Inactive => "inactive",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimePolicy {
    pub(crate) execute_local_actions: bool,
    pub(crate) apply_automatic_device_changes: bool,
    pub(crate) preview_local_actions: bool,
}

impl RuntimePolicy {
    const fn for_mode(mode: GraphicalSessionMode) -> Self {
        let local_active = matches!(mode, GraphicalSessionMode::LocalActive);
        Self {
            execute_local_actions: local_active,
            apply_automatic_device_changes: local_active,
            preview_local_actions: local_active,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionSnapshot {
    pub(crate) generation: u64,
    pub(crate) mode: GraphicalSessionMode,
    pub(crate) detail: String,
}

impl SessionSnapshot {
    pub(crate) fn policy(&self) -> RuntimePolicy {
        RuntimePolicy::for_mode(self.mode)
    }

    pub(crate) fn actions_paused(&self) -> bool {
        !self.policy().execute_local_actions
    }

    fn observed(mode: GraphicalSessionMode) -> Self {
        Self {
            generation: 1,
            mode,
            detail: mode_detail(mode).to_owned(),
        }
    }

    fn unavailable(error: impl std::fmt::Display) -> Self {
        Self {
            generation: 1,
            mode: GraphicalSessionMode::Unknown,
            detail: format!(
                "Desktop session state is unavailable; local input enhancements are paused: {error}"
            ),
        }
    }
}

#[derive(Clone)]
pub(crate) struct SessionObserver {
    shared: Arc<SharedSessionState>,
}

impl SessionObserver {
    pub(crate) fn start() -> Self {
        let initial = inspect();
        let shared = Arc::new(SharedSessionState::new(initial));
        let watcher_state = shared.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("dogi-session-observer".to_owned())
            .spawn(move || watch(watcher_state))
        {
            shared.publish(SessionSnapshot::unavailable(format!(
                "could not start the session observer: {error}"
            )));
        }
        Self { shared }
    }

    pub(crate) fn snapshot(&self) -> SessionSnapshot {
        self.shared.snapshot()
    }

    pub(crate) fn permits_actions(&self, generation: u64) -> bool {
        let snapshot = self.shared.snapshot();
        snapshot.generation == generation && snapshot.policy().execute_local_actions
    }

    pub(crate) fn permits_automatic_device_changes(&self, generation: u64) -> bool {
        let snapshot = self.shared.snapshot();
        snapshot.generation == generation && snapshot.policy().apply_automatic_device_changes
    }
}

pub(crate) fn inspect() -> SessionSnapshot {
    match Connection::system()
        .map_err(|error| format!("could not connect to system D-Bus: {error}"))
        .and_then(|connection| inspect_with_connection(&connection))
    {
        Ok(mode) => SessionSnapshot::observed(mode),
        Err(error) => SessionSnapshot::unavailable(error),
    }
}

#[derive(Debug)]
struct SharedSessionState {
    snapshot: Mutex<SessionSnapshot>,
}

impl SharedSessionState {
    fn new(snapshot: SessionSnapshot) -> Self {
        Self {
            snapshot: Mutex::new(snapshot),
        }
    }

    fn snapshot(&self) -> SessionSnapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn publish(&self, mut next: SessionSnapshot) {
        let mut current = self
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if current.mode == next.mode && current.detail == next.detail {
            return;
        }
        next.generation = current.generation.wrapping_add(1).max(1);
        *current = next;
    }
}

fn watch(shared: Arc<SharedSessionState>) {
    loop {
        if let Err(error) = watch_connection(&shared) {
            shared.publish(SessionSnapshot::unavailable(error));
            std::thread::sleep(RECONNECT_DELAY);
        }
    }
}

fn watch_connection(shared: &SharedSessionState) -> std::result::Result<(), String> {
    let connection = Connection::system()
        .map_err(|error| format!("could not connect to system D-Bus: {error}"))?;
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(LOGIN1_DESTINATION)
        .map_err(|error| format!("could not subscribe to login session changes: {error}"))?
        .build();
    let messages = MessageIterator::for_match_rule(rule, &connection, Some(32))
        .map_err(|error| format!("could not subscribe to login session changes: {error}"))?;

    publish_inspection(shared, &connection)?;
    for message in messages {
        message.map_err(|error| format!("login session monitoring failed: {error}"))?;
        publish_inspection(shared, &connection)?;
    }
    Err("system D-Bus session monitoring ended unexpectedly".to_owned())
}

fn publish_inspection(
    shared: &SharedSessionState,
    connection: &Connection,
) -> std::result::Result<(), String> {
    let mode = inspect_with_connection(connection)?;
    shared.publish(SessionSnapshot::observed(mode));
    Ok(())
}

fn inspect_with_connection(
    connection: &Connection,
) -> std::result::Result<GraphicalSessionMode, String> {
    let manager = Proxy::new(
        connection,
        LOGIN1_DESTINATION,
        LOGIN1_MANAGER_PATH,
        LOGIN1_MANAGER_INTERFACE,
    )
    .map_err(|error| format!("could not create the login session manager: {error}"))?;
    let sessions: Vec<LoginSession> = manager
        .call("ListSessions", &())
        .map_err(|error| format!("could not enumerate login sessions: {error}"))?;
    // SAFETY: `geteuid` has no preconditions and does not dereference pointers.
    let uid = unsafe { libc::geteuid() };
    let mut graphical = Vec::new();

    for (_, session_uid, _, listed_seat, path) in sessions {
        if session_uid != uid {
            continue;
        }
        let session = Proxy::new(
            connection,
            LOGIN1_DESTINATION,
            path.as_str(),
            LOGIN1_SESSION_INTERFACE,
        )
        .map_err(|error| format!("could not inspect login session {path}: {error}"))?;
        let session_type: String = session
            .get_property("Type")
            .map_err(|error| format!("could not read the type of login session {path}: {error}"))?;
        if !is_graphical_session(&session_type) {
            continue;
        }
        graphical.push(SessionFacts {
            active: session.get_property("Active").map_err(|error| {
                format!("could not read the activity of login session {path}: {error}")
            })?,
            remote: session.get_property("Remote").map_err(|error| {
                format!("could not read the origin of login session {path}: {error}")
            })?,
            locked: session.get_property("LockedHint").map_err(|error| {
                format!("could not read the lock state of login session {path}: {error}")
            })?,
            has_seat: !listed_seat.is_empty(),
        });
    }

    Ok(classify_sessions(&graphical))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SessionFacts {
    active: bool,
    remote: bool,
    locked: bool,
    has_seat: bool,
}

fn classify_sessions(sessions: &[SessionFacts]) -> GraphicalSessionMode {
    let active = sessions
        .iter()
        .copied()
        .filter(|session| session.active)
        .collect::<Vec<_>>();
    if active.is_empty() {
        return GraphicalSessionMode::Inactive;
    }

    let remote = active.iter().any(|session| session.remote);
    let local = active
        .iter()
        .any(|session| !session.remote && session.has_seat);
    let ambiguous = active
        .iter()
        .any(|session| !session.remote && !session.has_seat);
    if ambiguous || (local && remote) {
        return GraphicalSessionMode::Unknown;
    }
    if remote {
        return GraphicalSessionMode::RemoteOnly;
    }
    if local && active.iter().all(|session| session.locked) {
        GraphicalSessionMode::LocalLocked
    } else if local {
        GraphicalSessionMode::LocalActive
    } else {
        GraphicalSessionMode::Unknown
    }
}

fn is_graphical_session(session_type: &str) -> bool {
    matches!(session_type, "wayland" | "x11" | "mir")
}

const fn mode_detail(mode: GraphicalSessionMode) -> &'static str {
    match mode {
        GraphicalSessionMode::LocalActive => "",
        GraphicalSessionMode::LocalLocked => {
            "The desktop is locked; local input enhancements are paused"
        }
        GraphicalSessionMode::RemoteOnly => {
            "Remote login is active; local input enhancements are paused"
        }
        GraphicalSessionMode::Inactive => {
            "No active local desktop; local input enhancements are paused"
        }
        GraphicalSessionMode::Unknown => {
            "Desktop session ownership is unclear; local input enhancements are paused"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL: SessionFacts = SessionFacts {
        active: true,
        remote: false,
        locked: false,
        has_seat: true,
    };
    const REMOTE: SessionFacts = SessionFacts {
        active: true,
        remote: true,
        locked: false,
        has_seat: false,
    };

    #[test]
    fn local_active_session_enables_runtime_mutations() {
        let mode = classify_sessions(&[LOCAL]);
        let policy = RuntimePolicy::for_mode(mode);

        assert_eq!(mode, GraphicalSessionMode::LocalActive);
        assert!(policy.execute_local_actions);
        assert!(policy.apply_automatic_device_changes);
        assert!(policy.preview_local_actions);
    }

    #[test]
    fn remote_only_session_is_observation_only() {
        let mode = classify_sessions(&[REMOTE]);
        let policy = RuntimePolicy::for_mode(mode);

        assert_eq!(mode, GraphicalSessionMode::RemoteOnly);
        assert!(!policy.execute_local_actions);
        assert!(!policy.apply_automatic_device_changes);
        assert!(!policy.preview_local_actions);
    }

    #[test]
    fn locked_local_session_is_observation_only() {
        let mode = classify_sessions(&[SessionFacts {
            locked: true,
            ..LOCAL
        }]);

        assert_eq!(mode, GraphicalSessionMode::LocalLocked);
        assert!(!RuntimePolicy::for_mode(mode).execute_local_actions);
    }

    #[test]
    fn concurrent_local_and_remote_sessions_fail_closed() {
        assert_eq!(
            classify_sessions(&[LOCAL, REMOTE]),
            GraphicalSessionMode::Unknown
        );
    }

    #[test]
    fn an_active_graphical_session_without_a_seat_or_remote_origin_is_ambiguous() {
        assert_eq!(
            classify_sessions(&[SessionFacts {
                has_seat: false,
                ..LOCAL
            }]),
            GraphicalSessionMode::Unknown
        );
    }

    #[test]
    fn inactive_sessions_do_not_enable_actions() {
        let mode = classify_sessions(&[SessionFacts {
            active: false,
            ..LOCAL
        }]);

        assert_eq!(mode, GraphicalSessionMode::Inactive);
        assert!(!RuntimePolicy::for_mode(mode).execute_local_actions);
    }

    #[test]
    fn a_session_transition_revokes_existing_action_generations() {
        let shared = Arc::new(SharedSessionState::new(SessionSnapshot::observed(
            GraphicalSessionMode::LocalActive,
        )));
        let observer = SessionObserver {
            shared: shared.clone(),
        };
        let generation = observer.snapshot().generation;
        assert!(observer.permits_actions(generation));
        assert!(observer.permits_automatic_device_changes(generation));

        shared.publish(SessionSnapshot::observed(GraphicalSessionMode::RemoteOnly));

        assert!(!observer.permits_actions(generation));
        assert!(!observer.permits_automatic_device_changes(generation));
        assert!(observer.snapshot().generation > generation);
    }
}
