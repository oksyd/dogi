use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::domain::{
    ActiveApplication, DeviceInfo, DogiError, Master3sRuntimeEvent, Master3sSettings,
    ResolvedRuntimeAction, Result, RuntimeActionResolver, SettingsApplyPlan, SettingsApplyReport,
    SettingsApplyStatus, ThumbWheelMode, ThumbWheelRuntimeAction,
    build_master3s_runtime_device_plan, device_settings_id, effective_master3s_settings_for_app,
    resolved_logitech_device_name,
};
use serde::{Deserialize, Serialize};

use crate::config::application::ApplicationConfigStore;
use crate::device::DeviceService;
use crate::environment::AppEnvironment;
use crate::persistence::{FileOwner, atomic_write, quarantine};

use super::actions::{
    RuntimeActionExecution, SystemRuntimeActionExecutor, execute_runtime_actions_guarded_with,
};
use super::battery::BatteryNotificationMonitor;
use super::control::{HorizontalScrollPreview, RuntimePreviewState, RuntimeReadiness};
use super::lock::ProcessLock;
use super::session::{SessionObserver, SessionSnapshot};

const PREVIEW_DIVERSION_SPEED_PERCENT: u16 = 101;
const TRANSIENT_RETRY_DELAY: Duration = Duration::from_secs(3);
const DEGRADED_RETRY_DELAY: Duration = Duration::from_secs(15);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const ACTIVE_DEVICE_STATE_VERSION: u8 = 1;

#[derive(Clone, Debug)]
pub(crate) struct RuntimeSupervisorOptions {
    pub(crate) device_id: Option<String>,
    pub(crate) max_events: Option<usize>,
    pub(crate) idle_timeout: Duration,
    pub(crate) execute_actions: bool,
    pub(crate) allow_device_write: bool,
}

struct RuntimeSessionState {
    battery_monitor: BatteryNotificationMonitor,
    active_device: ActiveDeviceSelector,
    failures: FailureTracker,
}

pub(crate) fn run(
    options: RuntimeSupervisorOptions,
    daemon: &DeviceService,
    environment: &AppEnvironment,
) -> Result<()> {
    log::info!("Background starting · {}", env!("CARGO_PKG_VERSION"));
    let _shutdown_signals = ShutdownSignalGuard::install()?;
    let _runtime_lock =
        ProcessLock::acquire(&environment.paths.global_runtime_lock, "action runtime")?;
    let control = RuntimePreviewState::start(&environment.paths)?;
    let session_observer = SessionObserver::start();
    let application_store = ApplicationConfigStore::for_environment(environment);
    let battery_state_path = environment.paths.battery_notification_state();
    let battery_monitor = BatteryNotificationMonitor::load(battery_state_path.clone())
        .unwrap_or_else(|error| {
            log::warn!("battery notification state was reset: {error}");
            BatteryNotificationMonitor::empty(battery_state_path)
        });
    let mut state = RuntimeSessionState {
        battery_monitor,
        active_device: ActiveDeviceSelector::for_environment(environment),
        failures: FailureTracker::default(),
    };

    if options.max_events.is_some() {
        loop {
            recover_runtime_transaction(daemon, &control)?;
            match run_session(
                &options,
                &control,
                &session_observer,
                &application_store,
                &mut state,
                daemon,
            )? {
                RuntimeSessionOutcome::Completed => return Ok(()),
                RuntimeSessionOutcome::SwitchDevice => continue,
            }
        }
    }

    loop {
        if shutdown_requested() {
            return Ok(());
        }
        let result = recover_runtime_transaction(daemon, &control).and_then(|()| {
            run_session(
                &options,
                &control,
                &session_observer,
                &application_store,
                &mut state,
                daemon,
            )
        });
        match result {
            Ok(RuntimeSessionOutcome::Completed) => return Ok(()),
            Ok(RuntimeSessionOutcome::SwitchDevice) => state.failures.reset(),
            Err(error) => {
                control.fail_pending(error.to_string());
                let failure = RuntimeFailure::classify(error);
                state.failures.publish(&control, &failure);
                wait_after_failure(state.failures.next_retry_delay(&failure), environment);
            }
        }
    }
}

fn recover_runtime_transaction(
    daemon: &DeviceService,
    control: &RuntimePreviewState,
) -> Result<()> {
    // Retry preserved snapshots before every session, including reconnects whose
    // new configuration no longer needs any runtime-owned hardware settings.
    control.publish_health(RuntimeReadiness::Starting, None);
    if daemon.recover_interrupted_runtime_transaction()?.is_some() {
        log::info!("Recovered interrupted device transaction");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeSessionOutcome {
    Completed,
    SwitchDevice,
}

fn run_session(
    options: &RuntimeSupervisorOptions,
    control: &RuntimePreviewState,
    session_observer: &SessionObserver,
    application_store: &ApplicationConfigStore,
    state: &mut RuntimeSessionState,
    daemon: &DeviceService,
) -> Result<RuntimeSessionOutcome> {
    let RuntimeSessionState {
        battery_monitor,
        active_device,
        failures,
    } = state;
    let preview_device_id = control.snapshot().preview.map(|preview| preview.device_id);
    let requested_device_id = options
        .device_id
        .as_deref()
        .or(preview_device_id.as_deref());
    let preview_overrode_auto_selection =
        options.device_id.is_none() && preview_device_id.is_some();
    let device_id = resolve_device_id(daemon, requested_device_id, active_device)?;
    let device = daemon.find_device(&device_id)?;
    let settings_id = device_settings_id(&device);
    let device_name = display_device_name(&device).to_owned();
    battery_monitor.activate(&settings_id);
    let mut base_settings = daemon.load_master3s_settings_for_device(&settings_id)?;
    let mut device_lease = RuntimeDeviceLease::new(
        daemon,
        &device_id,
        options.allow_device_write,
        control.clone(),
    );
    let mut processed_events = 0_usize;
    let mut focus_warning_printed = false;
    let mut active_effective_state = RuntimeEffectiveState::default();
    let mut preview_error: Option<(u64, String)> = None;
    let mut battery_runtime =
        BatteryRuntimeState::new(application_store.default_preferences().language);
    let mut listener = daemon.open_master3s_runtime_event_listener(&device_id)?;
    let mut action_executor = None;
    let mut action_resolver = RuntimeActionResolver::default();
    let mut session_generation = 0_u64;

    let outcome = (|| -> Result<RuntimeSessionOutcome> {
        log::info!("Background connected to mouse");
        log::info!(
            "  mode: actions {}, device writes {}",
            enabled(options.execute_actions),
            enabled(options.allow_device_write)
        );

        if options.max_events == Some(0) {
            return Ok(RuntimeSessionOutcome::Completed);
        }

        loop {
            if shutdown_requested() {
                return Ok(RuntimeSessionOutcome::Completed);
            }
            if reload_base_settings(daemon, &settings_id, &mut base_settings)? {
                log::info!("  settings reloaded");
            }

            let session_snapshot = session_observer.snapshot();
            synchronize_session(
                &session_snapshot,
                &mut session_generation,
                &mut action_resolver,
                &mut action_executor,
            );
            let policy = session_snapshot.policy();
            if !policy.apply_automatic_device_changes {
                device_lease.release()?;
                control.publish_health(RuntimeReadiness::Paused, None);
            }

            let active_application = policy
                .apply_automatic_device_changes
                .then(|| active_application(daemon, &mut focus_warning_printed))
                .flatten();
            let effective =
                effective_master3s_settings_for_app(&base_settings, active_application.as_ref());
            let matched_profile_name = effective
                .matched_profile
                .as_ref()
                .map(|profile| profile.name.clone());
            let preview_snapshot = control.snapshot();
            if preview_overrode_auto_selection
                && preview_snapshot
                    .preview
                    .as_ref()
                    .is_none_or(|preview| preview.device_id != device_id)
            {
                return Ok(RuntimeSessionOutcome::SwitchDevice);
            }
            if preview_error
                .as_ref()
                .is_some_and(|(generation, _)| *generation != preview_snapshot.generation)
            {
                preview_error = None;
            }
            let requested_preview = preview_snapshot
                .preview
                .as_ref()
                .filter(|preview| preview.device_id == device_id);
            let active_preview = requested_preview.filter(|_| policy.preview_local_actions);
            if requested_preview.is_some() && !policy.preview_local_actions {
                control
                    .publish_failed(preview_snapshot.generation, session_snapshot.detail.clone());
            }
            if let Some(preview) = preview_snapshot
                .preview
                .as_ref()
                .filter(|preview| preview.device_id != device_id)
            {
                if options.device_id.is_none() {
                    return Ok(RuntimeSessionOutcome::SwitchDevice);
                }
                control.publish_failed(
                    preview_snapshot.generation,
                    format!(
                        "preview device {} is not the active runtime device {}",
                        preview.device_id, device_id
                    ),
                );
            }

            let device_settings = runtime_device_settings(&effective.settings, active_preview);
            let device_plan = build_master3s_runtime_device_plan(
                &device_id,
                &device_settings,
                effective
                    .matched_profile
                    .as_ref()
                    .map(|profile| &profile.overrides),
            );
            let profile_changed = policy.apply_automatic_device_changes
                && active_effective_state.profile_name.as_deref()
                    != matched_profile_name.as_deref();
            if profile_changed {
                action_resolver.reset();
                log_profile_change(matched_profile_name.is_some());
            }
            let preview_transition = policy.preview_local_actions
                && active_effective_state.preview_active != active_preview.is_some();
            let device_reacquire =
                policy.apply_automatic_device_changes && !device_lease.owns_plan(&device_plan);
            let mut preview_apply_failed = false;
            if preview_transition || device_reacquire {
                if !session_observer.permits_automatic_device_changes(session_snapshot.generation) {
                    continue;
                }
                if options.allow_device_write {
                    match device_lease.apply_target(&device_settings, &device_plan) {
                        Ok(()) => {}
                        Err(error) => {
                            let detail = error.to_string();
                            if active_preview.is_some() || preview_transition {
                                control.publish_failed(preview_snapshot.generation, detail.clone());
                                preview_error = Some((preview_snapshot.generation, detail));
                                preview_apply_failed = true;
                            } else {
                                return Err(error);
                            }
                        }
                    }
                } else if active_preview.is_some() || preview_transition {
                    let detail = "horizontal scroll preview needs device-write access".to_owned();
                    control.publish_failed(preview_snapshot.generation, detail.clone());
                    preview_error = Some((preview_snapshot.generation, detail));
                    preview_apply_failed = true;
                }
            }
            if policy.apply_automatic_device_changes
                && !session_observer.permits_automatic_device_changes(session_snapshot.generation)
            {
                continue;
            }
            if policy.apply_automatic_device_changes
                && active_effective_state.needs_update(
                    matched_profile_name.as_deref(),
                    &device_settings,
                    active_preview.is_some(),
                )
            {
                active_effective_state.update(
                    matched_profile_name.clone(),
                    &device_settings,
                    active_preview.is_some(),
                );
            }

            let mut runtime_plan = daemon.plan_master3s_runtime(&effective.settings);
            if let Some(preview) = active_preview {
                runtime_plan.thumb_wheel = Some(ThumbWheelRuntimeAction::HorizontalScroll {
                    speed_percent: preview.speed_percent,
                });
            }
            if policy.preview_local_actions
                && !preview_apply_failed
                && (preview_snapshot.preview.is_none() || active_preview.is_some())
            {
                if let Some((_, detail)) = &preview_error {
                    control.publish_failed(preview_snapshot.generation, detail.clone());
                } else {
                    control.publish_applied(preview_snapshot.generation);
                }
            }
            if policy.execute_local_actions && !preview_apply_failed {
                control.publish_health(RuntimeReadiness::Ready, None);
            } else if preview_apply_failed {
                control.publish_health(
                    RuntimeReadiness::Degraded,
                    preview_error.as_ref().map(|(_, detail)| detail.clone()),
                );
            }

            let events =
                listener.read_events(1, battery_monitor.read_timeout(options.idle_timeout))?;
            maintain_battery_monitor(
                battery_monitor,
                &mut listener,
                application_store,
                &settings_id,
                &device_name,
                &mut battery_runtime,
            );
            for event in events {
                let execution_snapshot = session_observer.snapshot();
                synchronize_session(
                    &execution_snapshot,
                    &mut session_generation,
                    &mut action_resolver,
                    &mut action_executor,
                );
                let actions = if execution_snapshot.policy().execute_local_actions {
                    action_resolver.resolve(&runtime_plan, &event)
                } else {
                    Vec::new()
                };
                let executions = execute_actions(
                    options,
                    daemon,
                    session_observer,
                    &execution_snapshot,
                    &actions,
                    &mut action_executor,
                )?;
                log_event(&event, &actions, &executions, options.execute_actions);
                processed_events += 1;
                if options
                    .max_events
                    .is_some_and(|max_events| processed_events >= max_events)
                {
                    log::info!("Runtime service stopped after {processed_events} event(s)");
                    return Ok(RuntimeSessionOutcome::Completed);
                }
            }
            // Reset only after a successful session iteration (including an
            // idle read), not merely after opening the endpoint.
            failures.reset();
        }
    })();
    let restoration = device_lease.release();
    match (outcome, restoration) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(restoration)) => Err(restoration),
        (Err(error), Err(restoration)) => {
            log::warn!("temporary device settings restoration also failed: {restoration}");
            Err(error)
        }
    }
}

fn wait_for_retry(delay: Duration) {
    let deadline = std::time::Instant::now() + delay;
    while !shutdown_requested() {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(250)));
    }
}

fn wait_after_failure(retry_delay: Option<Duration>, environment: &AppEnvironment) {
    match retry_delay {
        Some(delay) => wait_for_retry(delay),
        None => wait_for_relevant_state_change(environment),
    }
}

fn wait_for_relevant_state_change(environment: &AppEnvironment) {
    let watched = [
        environment.paths.application_config(),
        environment.paths.device_settings(),
        environment.paths.device_transactions_dir(),
    ];
    let baseline: [Option<PathRevision>; 3] =
        std::array::from_fn(|index| path_revision(&watched[index]));
    while !shutdown_requested() {
        std::thread::sleep(Duration::from_millis(500));
        if watched
            .iter()
            .zip(baseline.iter())
            .any(|(path, baseline)| path_revision(path) != *baseline)
        {
            break;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PathRevision {
    length: u64,
    modified: Option<std::time::SystemTime>,
    is_directory: bool,
}

fn path_revision(path: &std::path::Path) -> Option<PathRevision> {
    std::fs::metadata(path).ok().map(|metadata| PathRevision {
        length: metadata.len(),
        modified: metadata.modified().ok(),
        is_directory: metadata.is_dir(),
    })
}

#[cfg(unix)]
static SHUTDOWN_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn shutdown_signal_handler(_signal: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(unix)]
struct ShutdownSignalGuard {
    previous_term: libc::sigaction,
    previous_int: libc::sigaction,
}

#[cfg(unix)]
impl ShutdownSignalGuard {
    fn install() -> Result<Self> {
        SHUTDOWN_REQUESTED.store(false, std::sync::atomic::Ordering::Relaxed);
        let previous_term = install_shutdown_handler(libc::SIGTERM)?;
        let previous_int = match install_shutdown_handler(libc::SIGINT) {
            Ok(previous) => previous,
            Err(error) => {
                // SAFETY: previous_term was returned by sigaction for SIGTERM in this process.
                let _ =
                    unsafe { libc::sigaction(libc::SIGTERM, &previous_term, std::ptr::null_mut()) };
                return Err(error);
            }
        };
        Ok(Self {
            previous_term,
            previous_int,
        })
    }
}

#[cfg(unix)]
impl Drop for ShutdownSignalGuard {
    fn drop(&mut self) {
        // SAFETY: both actions were returned by sigaction for these signals in this process.
        let _ =
            unsafe { libc::sigaction(libc::SIGTERM, &self.previous_term, std::ptr::null_mut()) };
        // SAFETY: both actions were returned by sigaction for these signals in this process.
        let _ = unsafe { libc::sigaction(libc::SIGINT, &self.previous_int, std::ptr::null_mut()) };
    }
}

#[cfg(unix)]
fn install_shutdown_handler(signal: libc::c_int) -> Result<libc::sigaction> {
    // SAFETY: zero is a valid initial representation for sigaction before all used fields and the
    // signal mask are initialized below.
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = shutdown_signal_handler as *const () as usize;
    // Do not restart a blocking HID poll: EINTR returns control to the supervisor so the routing
    // lease can be released immediately.
    action.sa_flags = 0;
    // SAFETY: action.sa_mask is a valid sigset_t owned by this stack frame.
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0 {
        return Err(DogiError::BackendUnavailable(format!(
            "failed to initialize shutdown signal mask: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: previous is an out-parameter and action is fully initialized for sigaction.
    let mut previous = unsafe { std::mem::zeroed::<libc::sigaction>() };
    // SAFETY: signal is SIGTERM or SIGINT, and both pointers refer to valid sigaction values.
    if unsafe { libc::sigaction(signal, &action, &mut previous) } != 0 {
        return Err(DogiError::BackendUnavailable(format!(
            "failed to install shutdown signal handler: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(previous)
}

#[cfg(unix)]
fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(not(unix))]
struct ShutdownSignalGuard;

#[cfg(not(unix))]
impl ShutdownSignalGuard {
    fn install() -> Result<Self> {
        Ok(Self)
    }
}

#[cfg(not(unix))]
fn shutdown_requested() -> bool {
    false
}

fn execute_actions(
    options: &RuntimeSupervisorOptions,
    daemon: &DeviceService,
    observer: &SessionObserver,
    snapshot: &SessionSnapshot,
    actions: &[ResolvedRuntimeAction],
    executor: &mut Option<SystemRuntimeActionExecutor>,
) -> Result<Vec<RuntimeActionExecution>> {
    if !options.execute_actions {
        return Ok(Vec::new());
    }
    if let Some(executor) = executor.as_mut() {
        return Ok(execute_runtime_actions_guarded_with(
            actions,
            executor,
            || observer.permits_actions(snapshot.generation),
        ));
    }
    if actions.iter().any(action_is_executable) {
        if !observer.permits_actions(snapshot.generation) {
            return Ok(Vec::new());
        }
        let executor = executor.insert(SystemRuntimeActionExecutor::open()?);
        return Ok(execute_runtime_actions_guarded_with(
            actions,
            executor,
            || observer.permits_actions(snapshot.generation),
        ));
    }
    daemon.execute_master3s_runtime_actions(actions)
}

fn maintain_battery_monitor(
    monitor: &mut BatteryNotificationMonitor,
    listener: &mut crate::hid::Master3sRuntimeEventListener,
    store: &ApplicationConfigStore,
    settings_id: &str,
    device_name: &str,
    runtime: &mut BatteryRuntimeState,
) {
    if monitor.preferences_due() {
        let preferences = match store.load_preferences() {
            Ok(preferences) => {
                runtime.config_warning.clear();
                preferences
            }
            Err(error) => {
                publish_once(
                    &mut runtime.config_warning,
                    error.to_string(),
                    "battery notification preferences are unavailable; using defaults",
                );
                store.default_preferences()
            }
        };
        runtime.language = preferences.language;
        if let Err(error) = monitor.update_preferences(
            settings_id,
            preferences.low_battery_notifications_enabled,
            preferences.full_battery_notifications_enabled,
        ) {
            publish_once(
                &mut runtime.monitor_warning,
                error.to_string(),
                "battery monitoring is temporarily unavailable",
            );
        }
    }
    if monitor.is_due() {
        match monitor.check_if_due(listener, settings_id, device_name, runtime.language) {
            Ok(()) => runtime.monitor_warning.clear(),
            Err(error) => publish_once(
                &mut runtime.monitor_warning,
                error.to_string(),
                "battery monitoring is temporarily unavailable",
            ),
        }
    }
}

struct BatteryRuntimeState {
    language: crate::ui::ApplicationLanguage,
    config_warning: String,
    monitor_warning: String,
}

impl BatteryRuntimeState {
    fn new(language: crate::ui::ApplicationLanguage) -> Self {
        Self {
            language,
            config_warning: String::new(),
            monitor_warning: String::new(),
        }
    }
}

fn publish_once(previous: &mut String, detail: String, context: &str) {
    if detail != *previous {
        log::warn!("{context}: {detail}");
        *previous = detail;
    }
}

struct RuntimeDeviceLease<'a> {
    daemon: &'a DeviceService,
    device_id: &'a str,
    writes_enabled: bool,
    owned_plan: Option<SettingsApplyPlan>,
    lease: Option<crate::device::RuntimeSettingsLease>,
    control: RuntimePreviewState,
    restoration_attempted: bool,
}

impl<'a> RuntimeDeviceLease<'a> {
    fn new(
        daemon: &'a DeviceService,
        device_id: &'a str,
        writes_enabled: bool,
        control: RuntimePreviewState,
    ) -> Self {
        Self {
            daemon,
            device_id,
            writes_enabled,
            owned_plan: None,
            lease: None,
            control,
            restoration_attempted: false,
        }
    }

    fn owns_plan(&self, plan: &SettingsApplyPlan) -> bool {
        !self.writes_enabled
            || self
                .owned_plan
                .as_ref()
                .is_some_and(|owned| same_runtime_hardware(owned, plan))
    }

    fn apply_target(&mut self, target: &Master3sSettings, plan: &SettingsApplyPlan) -> Result<()> {
        if !self.writes_enabled {
            return Ok(());
        }
        self.restoration_attempted = false;
        if plan.steps.is_empty() {
            self.daemon
                .release_runtime_settings_lease(&mut self.lease)?;
        } else {
            let report = self.daemon.replace_runtime_settings_lease(
                &mut self.lease,
                self.device_id,
                target,
                plan,
            )?;
            ensure_device_apply_succeeded(&report)?;
        }
        self.owned_plan = Some(plan.clone());
        Ok(())
    }

    fn release(&mut self) -> Result<()> {
        if self.restoration_attempted {
            return Ok(());
        }
        self.restoration_attempted = true;
        self.daemon
            .release_runtime_settings_lease(&mut self.lease)?;
        self.owned_plan = None;
        Ok(())
    }
}

impl Drop for RuntimeDeviceLease<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            let detail = format!("failed to restore temporary device settings: {error}");
            self.control
                .publish_health(RuntimeReadiness::Degraded, Some(detail.clone()));
            log::warn!("{detail}");
        }
    }
}

fn same_runtime_hardware(left: &SettingsApplyPlan, right: &SettingsApplyPlan) -> bool {
    use crate::domain::{ButtonAction, SettingsApplyOperation};
    left.steps.len() == right.steps.len()
        && left.steps.iter().zip(&right.steps).all(|(left, right)| {
            match (&left.operation, &right.operation) {
                (
                    SettingsApplyOperation::ThumbWheel { .. },
                    SettingsApplyOperation::ThumbWheel { .. },
                ) => true,
                (
                    SettingsApplyOperation::ButtonMapping {
                        button: a,
                        action: x,
                    },
                    SettingsApplyOperation::ButtonMapping {
                        button: b,
                        action: y,
                    },
                ) => a == b && (*x == ButtonAction::Gestures) == (*y == ButtonAction::Gestures),
                _ => left == right,
            }
        })
}

fn ensure_device_apply_succeeded(report: &SettingsApplyReport) -> Result<()> {
    let failures = report
        .outcomes
        .iter()
        .filter(|outcome| {
            matches!(
                outcome.status,
                SettingsApplyStatus::Failed
                    | SettingsApplyStatus::Unsupported
                    | SettingsApplyStatus::RolledBack
                    | SettingsApplyStatus::RollbackFailed
            )
        })
        .map(|outcome| {
            outcome
                .detail
                .clone()
                .unwrap_or_else(|| outcome.title.clone())
        })
        .collect::<Vec<_>>();
    if report.committed() && failures.is_empty() {
        Ok(())
    } else {
        let detail = if failures.is_empty() {
            format!("transaction ended in {:?}", report.transaction)
        } else {
            failures.join("; ")
        };
        Err(DogiError::BackendUnavailable(format!(
            "device settings could not be committed and verified: {detail}"
        )))
    }
}

fn runtime_device_settings(
    effective: &Master3sSettings,
    preview: Option<&HorizontalScrollPreview>,
) -> Master3sSettings {
    let mut settings = effective.clone();
    if preview.is_some() {
        settings.thumb_wheel = ThumbWheelMode::HorizontalScroll;
        settings.thumb_wheel_speed_percent = PREVIEW_DIVERSION_SPEED_PERCENT;
    }
    settings.normalized()
}

#[derive(Debug, Default)]
struct RuntimeEffectiveState {
    profile_name: Option<String>,
    settings: Option<Master3sSettings>,
    preview_active: bool,
}

impl RuntimeEffectiveState {
    fn needs_update(
        &self,
        profile_name: Option<&str>,
        settings: &Master3sSettings,
        preview_active: bool,
    ) -> bool {
        self.profile_name.as_deref() != profile_name
            || self.settings.as_ref() != Some(settings)
            || self.preview_active != preview_active
    }

    fn update(
        &mut self,
        profile_name: Option<String>,
        settings: &Master3sSettings,
        preview_active: bool,
    ) {
        self.profile_name = profile_name;
        self.settings = Some(settings.clone());
        self.preview_active = preview_active;
    }
}

fn synchronize_session(
    snapshot: &SessionSnapshot,
    generation: &mut u64,
    resolver: &mut RuntimeActionResolver,
    executor: &mut Option<SystemRuntimeActionExecutor>,
) {
    if *generation == snapshot.generation {
        return;
    }
    *generation = snapshot.generation;
    resolver.reset();
    *executor = None;
    if snapshot.detail.is_empty() {
        log::info!("  session: local input enhancements enabled");
    } else {
        log::info!("  session: {}", snapshot.detail);
    }
}

fn reload_base_settings(
    daemon: &DeviceService,
    settings_id: &str,
    current: &mut Master3sSettings,
) -> Result<bool> {
    let settings = daemon.load_master3s_settings_for_device(settings_id)?;
    if &settings == current {
        Ok(false)
    } else {
        *current = settings;
        Ok(true)
    }
}

fn active_application(
    daemon: &DeviceService,
    warning_printed: &mut bool,
) -> Option<ActiveApplication> {
    match daemon.active_application() {
        Ok(application) => application,
        Err(error) => {
            if !*warning_printed {
                log::warn!("active app profile detection unavailable: {error}");
                *warning_printed = true;
            }
            None
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredActiveDevice {
    version: u8,
    preference_key: String,
}

#[derive(Debug)]
struct ActiveDeviceSelector {
    path: Option<PathBuf>,
    owner: Option<FileOwner>,
    preference_key: Option<String>,
}

impl ActiveDeviceSelector {
    fn for_environment(environment: &AppEnvironment) -> Self {
        let owner = environment
            .user
            .uid
            .zip(environment.user.gid)
            .map(|(uid, gid)| FileOwner::new(uid, gid));
        Self::load(environment.paths.runtime_active_device(), owner)
    }

    fn load(path: PathBuf, owner: Option<FileOwner>) -> Self {
        let preference_key = match fs::read(&path) {
            Ok(contents) => match serde_json::from_slice::<StoredActiveDevice>(&contents) {
                Ok(stored)
                    if stored.version == ACTIVE_DEVICE_STATE_VERSION
                        && !stored.preference_key.trim().is_empty() =>
                {
                    Some(stored.preference_key)
                }
                Ok(_) | Err(_) => {
                    preserve_invalid_active_device_state(&path);
                    None
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                log::warn!(
                    "runtime active-device preference could not be read from {}: {error}",
                    path.display()
                );
                None
            }
        };
        Self {
            path: Some(path),
            owner,
            preference_key,
        }
    }

    #[cfg(test)]
    fn in_memory(preference_key: Option<&str>) -> Self {
        Self {
            path: None,
            owner: None,
            preference_key: preference_key.map(ToOwned::to_owned),
        }
    }

    fn select(&mut self, devices: &[DeviceInfo]) -> Option<String> {
        let mut candidates = runtime_device_candidates(devices);
        candidates.sort_by(|left, right| {
            runtime_device_rank(left)
                .cmp(&runtime_device_rank(right))
                .then_with(|| {
                    active_device_preference_key(left).cmp(&active_device_preference_key(right))
                })
                .then_with(|| left.id.cmp(&right.id))
        });
        let selected = self
            .preference_key
            .as_deref()
            .and_then(|preferred| {
                candidates
                    .iter()
                    .find(|device| active_device_preference_key(device) == preferred)
                    .copied()
            })
            .or_else(|| candidates.first().copied())?;
        let key = active_device_preference_key(selected);
        let id = selected.id.clone();
        if self.preference_key.as_deref() != Some(&key) {
            self.preference_key = Some(key);
            if let Err(error) = self.persist() {
                log::warn!("runtime active-device preference could not be saved: {error}");
            }
        }
        Some(id)
    }

    fn persist(&self) -> std::result::Result<(), String> {
        let (Some(path), Some(preference_key)) =
            (self.path.as_deref(), self.preference_key.as_deref())
        else {
            return Ok(());
        };
        let stored = StoredActiveDevice {
            version: ACTIVE_DEVICE_STATE_VERSION,
            preference_key: preference_key.to_owned(),
        };
        let mut bytes = serde_json::to_vec_pretty(&stored)
            .map_err(|error| format!("failed to encode active-device preference: {error}"))?;
        bytes.push(b'\n');
        atomic_write(path, &bytes, self.owner).map_err(|error| error.to_string())
    }
}

fn preserve_invalid_active_device_state(path: &Path) {
    match quarantine(path, "invalid-active-device") {
        Ok(Some(backup)) => log::warn!(
            "invalid runtime active-device preference was preserved at {}",
            backup.display()
        ),
        Ok(None) => {}
        Err(error) => log::warn!(
            "invalid runtime active-device preference at {} could not be preserved: {error}",
            path.display()
        ),
    }
}

fn runtime_device_candidates(devices: &[DeviceInfo]) -> Vec<&DeviceInfo> {
    devices
        .iter()
        .filter(|device| {
            device.is_logitech()
                && device.paired_device.is_some()
                && device.report_descriptor.is_hidpp_interface()
        })
        .collect()
}

fn active_device_preference_key(device: &DeviceInfo) -> String {
    device_settings_id(device)
}

fn runtime_device_rank(device: &DeviceInfo) -> u8 {
    let resolved_name = resolved_logitech_device_name(device).unwrap_or_default();
    if resolved_name.starts_with("MX Master 3S") {
        0
    } else if device
        .paired_device
        .as_ref()
        .and_then(|paired| paired.kind.as_deref())
        .is_some_and(|kind| kind.eq_ignore_ascii_case("mouse"))
    {
        1
    } else {
        2
    }
}

fn resolve_device_id(
    daemon: &DeviceService,
    requested: Option<&str>,
    selector: &mut ActiveDeviceSelector,
) -> Result<String> {
    if let Some(requested) = requested {
        let requested = requested.trim();
        if requested.is_empty() {
            return Err(DogiError::InvalidArgument(
                "--device-id cannot be empty".to_owned(),
            ));
        }
        return Ok(requested.to_owned());
    }
    let devices = daemon.scan_devices()?;
    selector.select(&devices).ok_or(DogiError::DeviceNotFound)
}

fn display_device_name(device: &DeviceInfo) -> &str {
    resolved_logitech_device_name(device).unwrap_or(&device.name)
}

fn action_is_executable(action: &ResolvedRuntimeAction) -> bool {
    !matches!(
        action.command,
        crate::domain::RuntimeCommand::Noop | crate::domain::RuntimeCommand::Unsupported
    )
}

fn log_profile_change(application_profile: bool) {
    // Neither window titles nor user-defined profile names belong in logs.
    log::info!(
        "Active profile changed: {}",
        if application_profile {
            "application profile"
        } else {
            "default profile"
        }
    );
}

fn log_event(
    event: &Master3sRuntimeEvent,
    actions: &[ResolvedRuntimeAction],
    executions: &[RuntimeActionExecution],
    execute_actions: bool,
) {
    log::debug!("Input event: {event:?}");
    for action in actions {
        log::debug!("Resolved action: {}", action.command.label());
    }
    if execute_actions {
        for execution in executions {
            log::debug!("Action execution: {}", execution.status.label());
        }
    }
}

fn enabled(value: bool) -> &'static str {
    if value { "enabled" } else { "disabled" }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeFailureKind {
    Transient,
    Degraded,
    Terminal,
}

struct RuntimeFailure {
    kind: RuntimeFailureKind,
    detail: String,
}

impl RuntimeFailure {
    fn classify(error: DogiError) -> Self {
        let kind = match error {
            DogiError::DeviceNotFound
            | DogiError::BackendUnavailable(_)
            | DogiError::Transport(_) => RuntimeFailureKind::Transient,
            DogiError::Protocol(_) => RuntimeFailureKind::Degraded,
            DogiError::Config(_)
            | DogiError::InvalidArgument(_)
            | DogiError::Ui(_)
            | DogiError::UnsupportedFeature(_) => RuntimeFailureKind::Terminal,
        };
        Self {
            kind,
            detail: error.to_string(),
        }
    }

    fn retry_delay(&self) -> Option<Duration> {
        match self.kind {
            RuntimeFailureKind::Transient => Some(TRANSIENT_RETRY_DELAY),
            RuntimeFailureKind::Degraded => Some(DEGRADED_RETRY_DELAY),
            RuntimeFailureKind::Terminal => None,
        }
    }

    fn readiness(&self) -> RuntimeReadiness {
        match self.kind {
            RuntimeFailureKind::Transient => RuntimeReadiness::Reconnecting,
            RuntimeFailureKind::Degraded => RuntimeReadiness::Degraded,
            RuntimeFailureKind::Terminal => RuntimeReadiness::Terminal,
        }
    }

    fn label(&self) -> &'static str {
        match self.kind {
            RuntimeFailureKind::Transient => "waiting for the device",
            RuntimeFailureKind::Degraded => "running in degraded mode",
            RuntimeFailureKind::Terminal => "configuration requires attention",
        }
    }
}

#[derive(Default)]
struct FailureTracker {
    previous: String,
    repetitions: u32,
    retry_delay: Option<Duration>,
}

impl FailureTracker {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn next_retry_delay(&mut self, failure: &RuntimeFailure) -> Option<Duration> {
        // Error text or category can vary between attempts without indicating
        // recovery. Only session progress resets the consecutive-failure delay.
        self.retry_delay = failure.retry_delay().map(|minimum| {
            self.retry_delay
                .map_or(minimum, |previous| previous.saturating_mul(2).max(minimum))
                .min(MAX_RETRY_DELAY)
        });
        self.retry_delay
    }

    fn publish(&mut self, control: &RuntimePreviewState, failure: &RuntimeFailure) {
        control.publish_health(failure.readiness(), Some(failure.detail.clone()));
        if self.previous == failure.detail {
            self.repetitions = self.repetitions.saturating_add(1);
            if self.repetitions.is_multiple_of(20) {
                log::warn!(
                    "Dogi runtime is still {} ({} attempts): {}",
                    failure.label(),
                    self.repetitions,
                    failure.detail
                );
            }
        } else {
            log::warn!("Dogi runtime is {}: {}", failure.label(), failure.detail);
            self.previous.clone_from(&failure.detail);
            self.repetitions = 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::control::RuntimeHealthSnapshot;
    use super::*;
    use crate::domain::HidppFeature;

    #[test]
    fn horizontal_speed_changes_reuse_routing_without_touching_vertical_settings() {
        let mut settings = Master3sSettings {
            thumb_wheel_speed_percent: 400,
            ..Master3sSettings::default()
        };
        let first = build_master3s_runtime_device_plan("device", &settings, None);
        settings.thumb_wheel_speed_percent = 300;
        let next = build_master3s_runtime_device_plan("device", &settings, None);
        assert_eq!(next.steps.len(), 1);
        assert_eq!(next.steps[0].feature, HidppFeature::ThumbWheel);
        assert!(same_runtime_hardware(&first, &next));
        settings.thumb_wheel_speed_percent = 100;
        let native = build_master3s_runtime_device_plan("device", &settings, None);
        assert!(native.steps.is_empty());
        assert!(!same_runtime_hardware(&next, &native));
    }

    #[test]
    fn horizontal_preview_only_acquires_thumb_wheel_routing() {
        let settings = Master3sSettings::default();
        let preview = HorizontalScrollPreview {
            lease_id: "test-preview".to_owned(),
            device_id: "device".to_owned(),
            speed_percent: 100,
        };
        let target = runtime_device_settings(&settings, Some(&preview));
        let plan = build_master3s_runtime_device_plan("device", &target, None);
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].feature, HidppFeature::ThumbWheel);
        let after = build_master3s_runtime_device_plan(
            "device",
            &runtime_device_settings(&settings, None),
            None,
        );
        assert!(after.steps.is_empty());
    }

    #[test]
    fn changing_profile_pointer_override_requires_a_new_hardware_plan() {
        let settings = Master3sSettings {
            pointer_speed_percent: 150,
            ..Master3sSettings::default()
        };
        let overrides = crate::domain::AppProfileOverrides {
            pointer_speed_percent: Some(150),
            ..Default::default()
        };
        let first = build_master3s_runtime_device_plan("device", &settings, Some(&overrides));
        let settings = Master3sSettings {
            pointer_speed_percent: 125,
            ..settings
        };
        let overrides = crate::domain::AppProfileOverrides {
            pointer_speed_percent: Some(125),
            ..overrides
        };
        let next = build_master3s_runtime_device_plan("device", &settings, Some(&overrides));
        assert!(!same_runtime_hardware(&first, &next));
    }

    #[test]
    fn unsupported_capabilities_do_not_trigger_disconnect_retries() {
        let failure = RuntimeFailure::classify(DogiError::UnsupportedFeature(
            "pointer sensitivity is unavailable".to_owned(),
        ));
        assert_eq!(failure.kind, RuntimeFailureKind::Terminal);
        assert_eq!(failure.readiness(), RuntimeReadiness::Terminal);
        assert_eq!(failure.retry_delay(), None);
    }

    #[test]
    fn failure_classes_have_distinct_health_and_backoff() {
        let terminal = RuntimeFailure::classify(DogiError::Config("invalid schema".to_owned()));
        let transient = RuntimeFailure::classify(DogiError::DeviceNotFound);

        assert_eq!(terminal.kind, RuntimeFailureKind::Terminal);
        assert_eq!(terminal.readiness(), RuntimeReadiness::Terminal);
        assert_eq!(terminal.retry_delay(), None);
        assert_eq!(transient.kind, RuntimeFailureKind::Transient);
        assert_eq!(transient.retry_delay(), Some(TRANSIENT_RETRY_DELAY));
    }

    #[test]
    fn repeated_connection_failures_back_off_with_a_bounded_delay() {
        let mut tracker = FailureTracker::default();
        let failure = RuntimeFailure::classify(DogiError::DeviceNotFound);

        for seconds in [3, 6, 12, 24, 30, 30] {
            assert_eq!(
                tracker.next_retry_delay(&failure),
                Some(Duration::from_secs(seconds))
            );
        }
        for _ in 0..1_000 {
            assert_eq!(tracker.next_retry_delay(&failure), Some(MAX_RETRY_DELAY));
        }
    }

    #[test]
    fn changing_errors_do_not_reset_connection_backoff() {
        let mut tracker = FailureTracker::default();
        for (error, seconds) in [
            (DogiError::DeviceNotFound, 3),
            (DogiError::Transport("endpoint busy".to_owned()), 6),
            (DogiError::Protocol("invalid reply".to_owned()), 15),
            (DogiError::Transport("device not responding".to_owned()), 30),
        ] {
            assert_eq!(
                tracker.next_retry_delay(&RuntimeFailure::classify(error)),
                Some(Duration::from_secs(seconds))
            );
        }
    }

    #[test]
    fn recovered_session_resets_retry_delay_and_error_history() {
        let failure = RuntimeFailure::classify(DogiError::DeviceNotFound);
        let mut tracker = FailureTracker {
            previous: failure.detail.clone(),
            repetitions: 20,
            retry_delay: Some(MAX_RETRY_DELAY),
        };

        tracker.reset();

        assert!(tracker.previous.is_empty());
        assert_eq!(tracker.repetitions, 0);
        assert_eq!(
            tracker.next_retry_delay(&failure),
            Some(TRANSIENT_RETRY_DELAY)
        );
    }

    #[test]
    fn terminal_failure_waits_for_changes_instead_of_retrying() {
        let mut tracker = FailureTracker::default();
        let transient = RuntimeFailure::classify(DogiError::DeviceNotFound);
        let terminal = RuntimeFailure::classify(DogiError::Config("invalid schema".to_owned()));

        tracker.next_retry_delay(&transient);
        assert_eq!(tracker.next_retry_delay(&terminal), None);
        assert_eq!(
            tracker.next_retry_delay(&transient),
            Some(TRANSIENT_RETRY_DELAY)
        );
    }

    #[test]
    fn readiness_snapshot_marks_only_ready_state_as_ready() {
        let ready = RuntimeHealthSnapshot {
            version: "1".to_owned(),
            readiness: RuntimeReadiness::Ready,
            degraded: false,
            last_error: None,
        };
        assert!(ready.ready());
    }

    #[test]
    fn rolled_back_device_transaction_is_not_accepted_as_applied() {
        let report = SettingsApplyReport {
            device_id: "device".to_owned(),
            profile_name: "profile".to_owned(),
            transaction: crate::domain::SettingsTransactionState::RolledBack,
            outcomes: vec![crate::domain::SettingsApplyOutcome {
                title: "Set pointer speed".to_owned(),
                feature: HidppFeature::PointerSpeed,
                status: SettingsApplyStatus::RolledBack,
                detail: Some("original value restored".to_owned()),
            }],
        };

        assert!(ensure_device_apply_succeeded(&report).is_err());
    }

    #[test]
    fn same_receiver_dual_slots_choose_one_stable_event_consumer() {
        let first = runtime_device("receiver-a", 1, "UNIT-B");
        let second = runtime_device("receiver-a", 2, "UNIT-A");
        let mut selector = ActiveDeviceSelector::in_memory(None);

        let selected = selector
            .select(&[first.clone(), second.clone()])
            .expect("a runtime device is selected");

        assert_eq!(selected, second.id);
        assert_eq!(
            crate::domain::hidpp_endpoint_id(&first.id),
            crate::domain::hidpp_endpoint_id(&second.id)
        );
    }

    #[test]
    fn dual_receivers_keep_the_persisted_active_device() {
        let directory = std::env::temp_dir().join(format!(
            "dogi-runtime-device-selector-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let path = directory.join("active.json");
        let preferred = runtime_device("receiver-b", 1, "UNIT-B");
        let other = runtime_device("receiver-a", 1, "UNIT-A");
        let mut initial = ActiveDeviceSelector::load(path.clone(), None);
        assert_eq!(
            initial.select(std::slice::from_ref(&preferred)),
            Some(preferred.id.clone())
        );

        let mut restarted = ActiveDeviceSelector::load(path, None);
        let selected = restarted
            .select(&[other, preferred.clone()])
            .expect("persisted receiver is selected");

        assert_eq!(selected, preferred.id);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn active_device_disconnect_falls_back_and_does_not_flap_on_reconnect() {
        let preferred = runtime_device("receiver-a", 1, "UNIT-A");
        let fallback = runtime_device("receiver-b", 1, "UNIT-B");
        let mut selector = ActiveDeviceSelector::in_memory(Some("046d:unit:UNITA"));
        assert_eq!(
            selector.select(&[fallback.clone(), preferred.clone()]),
            Some(preferred.id.clone())
        );

        assert_eq!(
            selector.select(std::slice::from_ref(&fallback)),
            Some(fallback.id.clone())
        );
        assert_eq!(
            selector.select(&[preferred, fallback.clone()]),
            Some(fallback.id)
        );
    }

    fn runtime_device(endpoint: &str, slot: u8, unit_id: &str) -> DeviceInfo {
        DeviceInfo {
            id: format!("{endpoint}:slot:{slot:02x}:wpid:b034"),
            name: "Logitech Bolt Receiver".to_owned(),
            paired_device: Some(crate::domain::PairedDeviceInfo {
                slot,
                name: Some("MX Master 3S".to_owned()),
                kind: Some("mouse".to_owned()),
                wpid: Some("B034".to_owned()),
                protocol: None,
                unit_id: Some(unit_id.to_owned()),
                model_id: Some("B034".to_owned()),
                feature_count: 0,
                features_complete: true,
                features: Vec::new(),
            }),
            manufacturer: Some("Logitech".to_owned()),
            serial_number: Some(format!("SERIAL-{endpoint}")),
            bus: crate::domain::BusKind::Usb,
            bus_id: Some(0x0003),
            vendor_id: 0x046d,
            product_id: 0xc548,
            release_number: None,
            connection: crate::domain::ConnectionKind::Bolt,
            receiver_kind: Some(crate::domain::ReceiverKind::Bolt),
            path: format!("/dev/{endpoint}"),
            sysfs_path: format!("/sys/class/hidraw/{endpoint}"),
            physical_path: Some(format!("usb-{endpoint}")),
            driver: Some("hid-generic".to_owned()),
            interface_number: Some(2),
            usage_page: Some(0xff00),
            usage: Some(0x0001),
            access: crate::domain::DeviceAccess::default(),
            battery: crate::domain::BatteryInfo::not_queried("test"),
            report_descriptor: crate::domain::ReportDescriptorInfo {
                hidpp_usage: Some(crate::domain::HidUsage {
                    usage_page: 0xff00,
                    usage: 0x0001,
                }),
                ..crate::domain::ReportDescriptorInfo::default()
            },
            capabilities: crate::domain::DeviceCapabilities::default(),
        }
    }
}
