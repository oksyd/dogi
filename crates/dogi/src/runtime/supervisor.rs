use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dogi_core::{
    ActiveApplication, ButtonAction, DeviceInfo, DogiError, HidppFeature, Master3sButton,
    Master3sRuntimeEvent, Master3sSettings, ResolvedRuntimeAction, Result, RuntimeActionResolver,
    SettingsApplyPlan, SettingsApplyReport, SettingsApplyStatus, SettingsApplyStep, ThumbWheelMode,
    ThumbWheelRuntimeAction, build_master3s_apply_plan, build_master3s_device_diff_plan,
    device_settings_id, effective_master3s_settings_for_app, resolved_logitech_device_name,
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
const ACTIVE_DEVICE_STATE_VERSION: u8 = 1;

#[derive(Clone, Debug)]
pub(crate) struct RuntimeSupervisorOptions {
    pub(crate) device_id: Option<String>,
    pub(crate) max_events: Option<usize>,
    pub(crate) idle_timeout: Duration,
    pub(crate) execute_actions: bool,
    pub(crate) allow_device_write: bool,
}

pub(crate) fn run(
    options: RuntimeSupervisorOptions,
    daemon: &DeviceService,
    environment: &AppEnvironment,
) -> Result<()> {
    let _shutdown_signals = ShutdownSignalGuard::install()?;
    let _runtime_lock =
        ProcessLock::acquire(&environment.paths.global_runtime_lock, "action runtime")?;
    let control = RuntimePreviewState::start(&environment.paths)?;
    let session_observer = SessionObserver::start();
    let application_store = ApplicationConfigStore::for_environment(environment);
    let battery_state_path = environment.paths.battery_notification_state();
    let mut battery_monitor = BatteryNotificationMonitor::load(battery_state_path.clone())
        .unwrap_or_else(|error| {
            eprintln!("battery notification state was reset: {error}");
            BatteryNotificationMonitor::empty(battery_state_path)
        });
    let mut recovery_complete = false;
    let mut active_device = ActiveDeviceSelector::for_environment(environment);

    if options.max_events.is_some() {
        recover_runtime_transaction(daemon, &control, &mut recovery_complete)?;
        loop {
            match run_session(
                &options,
                &control,
                &session_observer,
                &application_store,
                &mut battery_monitor,
                &mut active_device,
                daemon,
            )? {
                RuntimeSessionOutcome::Completed => return Ok(()),
                RuntimeSessionOutcome::SwitchDevice => continue,
            }
        }
    }

    let mut failures = FailureTracker::default();
    loop {
        if shutdown_requested() {
            return Ok(());
        }
        let result = recover_runtime_transaction(daemon, &control, &mut recovery_complete)
            .and_then(|()| {
                run_session(
                    &options,
                    &control,
                    &session_observer,
                    &application_store,
                    &mut battery_monitor,
                    &mut active_device,
                    daemon,
                )
            });
        match result {
            Ok(RuntimeSessionOutcome::Completed) => return Ok(()),
            Ok(RuntimeSessionOutcome::SwitchDevice) => failures.reset(),
            Err(error) => {
                control.fail_pending(error.to_string());
                let failure = RuntimeFailure::classify(error);
                failures.publish(&control, &failure);
                wait_after_failure(&failure, environment);
            }
        }
    }
}

fn recover_runtime_transaction(
    daemon: &DeviceService,
    control: &RuntimePreviewState,
    complete: &mut bool,
) -> Result<()> {
    if *complete {
        return Ok(());
    }
    control.publish_health(RuntimeReadiness::Starting, None);
    if let Some(report) = daemon.recover_interrupted_runtime_transaction()? {
        println!(
            "Recovered interrupted device transaction: {:?}",
            report.transaction
        );
    }
    *complete = true;
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
    battery_monitor: &mut BatteryNotificationMonitor,
    active_device: &mut ActiveDeviceSelector,
    daemon: &DeviceService,
) -> Result<RuntimeSessionOutcome> {
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
        &base_settings,
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
        println!("Runtime service for {device_id}");
        println!(
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
                device_lease.update_base_settings(&base_settings);
                println!("  settings reloaded");
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
                device_lease.release_base()?;
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
            let profile_changed = policy.apply_automatic_device_changes
                && active_effective_state.profile_name.as_deref()
                    != matched_profile_name.as_deref();
            if profile_changed {
                action_resolver.reset();
                print_profile_change(active_application.as_ref(), matched_profile_name.as_deref());
            }
            let preview_transition = policy.preview_local_actions
                && active_effective_state.preview_active != active_preview.is_some();
            let target_changed = policy.apply_automatic_device_changes
                && active_effective_state.settings.as_ref() != Some(&device_settings);
            let device_reacquire = policy.apply_automatic_device_changes
                && !device_lease.owns_target(&device_settings);
            let mut preview_apply_failed = false;
            if target_changed || preview_transition || device_reacquire {
                if !session_observer.permits_automatic_device_changes(session_snapshot.generation) {
                    continue;
                }
                if options.allow_device_write {
                    match device_lease.apply_target(&device_settings, preview_transition) {
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
            if events.is_empty() {
                continue;
            }

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
                print_event(&event, &actions, &executions, options.execute_actions);
                processed_events += 1;
                if options
                    .max_events
                    .is_some_and(|max_events| processed_events >= max_events)
                {
                    println!("Runtime service stopped after {processed_events} event(s)");
                    return Ok(RuntimeSessionOutcome::Completed);
                }
            }
        }
    })();
    let restoration = device_lease.release_base();
    match (outcome, restoration) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(restoration)) => Err(DogiError::BackendUnavailable(format!(
            "failed to restore base device settings: {restoration}"
        ))),
        (Err(error), Err(restoration)) => Err(DogiError::BackendUnavailable(format!(
            "{error}; base device settings restoration also failed: {restoration}"
        ))),
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

fn wait_after_failure(failure: &RuntimeFailure, environment: &AppEnvironment) {
    match failure.retry_delay() {
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
    listener: &mut dogi_hid::Master3sRuntimeEventListener,
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
    language: dogi_ui::ApplicationLanguage,
    config_warning: String,
    monitor_warning: String,
}

impl BatteryRuntimeState {
    fn new(language: dogi_ui::ApplicationLanguage) -> Self {
        Self {
            language,
            config_warning: String::new(),
            monitor_warning: String::new(),
        }
    }
}

fn publish_once(previous: &mut String, detail: String, context: &str) {
    if detail != *previous {
        eprintln!("{context}: {detail}");
        *previous = detail;
    }
}

struct RuntimeDeviceLease<'a> {
    daemon: &'a DeviceService,
    device_id: &'a str,
    writes_enabled: bool,
    state: RuntimeDeviceLeaseState,
    control: RuntimePreviewState,
}

impl<'a> RuntimeDeviceLease<'a> {
    fn new(
        daemon: &'a DeviceService,
        device_id: &'a str,
        writes_enabled: bool,
        base_settings: &Master3sSettings,
        control: RuntimePreviewState,
    ) -> Self {
        Self {
            daemon,
            device_id,
            writes_enabled,
            state: RuntimeDeviceLeaseState::new(base_settings),
            control,
        }
    }

    fn update_base_settings(&mut self, settings: &Master3sSettings) {
        self.state.update_base_settings(settings);
    }

    fn owns_target(&self, target: &Master3sSettings) -> bool {
        !self.writes_enabled || self.state.owns_target(target)
    }

    fn apply_target(&mut self, target: &Master3sSettings, force_thumb_wheel: bool) -> Result<()> {
        if !self.writes_enabled {
            return Ok(());
        }
        let (target, plan) = self
            .state
            .target_plan(self.device_id, target, force_thumb_wheel);
        if !plan.steps.is_empty() {
            let report =
                self.daemon
                    .apply_master3s_settings_plan(self.device_id, &target, &plan)?;
            ensure_device_apply_succeeded(&report)?;
        }
        self.state.commit(target);
        Ok(())
    }

    fn release_base(&mut self) -> Result<()> {
        if !self.writes_enabled {
            return Ok(());
        }
        let Some((target, plan)) = self.state.release_plan(self.device_id) else {
            return Ok(());
        };
        if !plan.steps.is_empty() {
            let report =
                self.daemon
                    .apply_master3s_settings_plan(self.device_id, &target, &plan)?;
            ensure_device_apply_succeeded(&report)?;
        }
        self.state.commit(target);
        Ok(())
    }
}

#[derive(Debug)]
struct RuntimeDeviceLeaseState {
    base_settings: Master3sSettings,
    owned_target: Option<Master3sSettings>,
}

impl RuntimeDeviceLeaseState {
    fn new(base_settings: &Master3sSettings) -> Self {
        Self {
            base_settings: base_settings.normalized(),
            owned_target: None,
        }
    }

    fn update_base_settings(&mut self, settings: &Master3sSettings) {
        self.base_settings = settings.normalized();
    }

    fn owns_target(&self, target: &Master3sSettings) -> bool {
        self.owned_target.as_ref() == Some(&target.normalized())
    }

    fn target_plan(
        &self,
        device_id: &str,
        target: &Master3sSettings,
        force_thumb_wheel: bool,
    ) -> (Master3sSettings, SettingsApplyPlan) {
        let target = target.normalized();
        let plan = runtime_device_apply_plan(
            device_id,
            self.owned_target.as_ref(),
            &target,
            force_thumb_wheel,
        );
        (target, plan)
    }

    fn release_plan(&self, device_id: &str) -> Option<(Master3sSettings, SettingsApplyPlan)> {
        let target = released_device_settings(&self.base_settings);
        if self.owned_target.as_ref() == Some(&target) {
            return None;
        }
        let plan = runtime_device_apply_plan(device_id, self.owned_target.as_ref(), &target, false);
        Some((target, plan))
    }

    fn commit(&mut self, target: Master3sSettings) {
        self.owned_target = Some(target);
    }
}

impl Drop for RuntimeDeviceLease<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.release_base() {
            let detail = format!("failed to restore base device settings: {error}");
            self.control
                .publish_health(RuntimeReadiness::Degraded, Some(detail.clone()));
            eprintln!("{detail}");
        }
    }
}

fn runtime_device_apply_plan(
    device_id: &str,
    baseline: Option<&Master3sSettings>,
    target: &Master3sSettings,
    force_thumb_wheel: bool,
) -> SettingsApplyPlan {
    let mut plan = baseline.map_or_else(
        || {
            let mut plan = build_master3s_apply_plan(device_id, target);
            plan.steps.retain(|step| step.requires_device_write);
            plan
        },
        |baseline| build_master3s_device_diff_plan(device_id, baseline, target),
    );
    if force_thumb_wheel {
        let forced = build_master3s_apply_plan(device_id, target);
        merge_missing_steps(
            &mut plan,
            forced
                .steps
                .into_iter()
                .filter(|step| step.feature == HidppFeature::ThumbWheel),
        );
    }
    plan
}

fn merge_missing_steps(
    plan: &mut SettingsApplyPlan,
    steps: impl IntoIterator<Item = SettingsApplyStep>,
) {
    for step in steps {
        let exists = plan
            .steps
            .iter()
            .any(|current| match (&current.operation, &step.operation) {
                (
                    dogi_core::SettingsApplyOperation::ButtonMapping { button: left, .. },
                    dogi_core::SettingsApplyOperation::ButtonMapping { button: right, .. },
                ) => left == right,
                _ => current.feature == step.feature,
            });
        if !exists {
            plan.steps.push(step);
        }
    }
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

fn released_device_settings(base: &Master3sSettings) -> Master3sSettings {
    let mut settings = base.normalized();
    settings.thumb_wheel = ThumbWheelMode::HorizontalScroll;
    settings.thumb_wheel_speed_percent = dogi_core::DEFAULT_THUMB_WHEEL_SPEED_PERCENT;
    for button in Master3sButton::ALL {
        settings.set_button_action(button, ButtonAction::Native);
    }
    settings.normalized()
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
        println!("  session: local input enhancements enabled");
    } else {
        println!("  session: {}", snapshot.detail);
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
                eprintln!("active app profile detection unavailable: {error}");
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
                eprintln!(
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
                eprintln!("runtime active-device preference could not be saved: {error}");
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
        Ok(Some(backup)) => eprintln!(
            "invalid runtime active-device preference was preserved at {}",
            backup.display()
        ),
        Ok(None) => {}
        Err(error) => eprintln!(
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
        dogi_core::RuntimeCommand::Noop | dogi_core::RuntimeCommand::Unsupported
    )
}

fn print_profile_change(application: Option<&ActiveApplication>, profile: Option<&str>) {
    let application = application
        .map(ActiveApplication::summary)
        .filter(|summary| !summary.is_empty())
        .unwrap_or_else(|| "unknown application".to_owned());
    println!("  active app: {application}");
    println!("  active profile: {}", profile.unwrap_or("default profile"));
}

fn print_event(
    event: &Master3sRuntimeEvent,
    actions: &[ResolvedRuntimeAction],
    executions: &[RuntimeActionExecution],
    execute_actions: bool,
) {
    println!("  event: {event:?}");
    for action in actions {
        println!("    action: {}", action.command.label());
    }
    if execute_actions {
        for execution in executions {
            println!("    execution: {}", execution.status.label());
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
            DogiError::Protocol(_) | DogiError::UnsupportedFeature(_) => {
                RuntimeFailureKind::Degraded
            }
            DogiError::Config(_) | DogiError::InvalidArgument(_) | DogiError::Ui(_) => {
                RuntimeFailureKind::Terminal
            }
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
}

impl FailureTracker {
    fn reset(&mut self) {
        self.previous.clear();
        self.repetitions = 0;
    }

    fn publish(&mut self, control: &RuntimePreviewState, failure: &RuntimeFailure) {
        control.publish_health(failure.readiness(), Some(failure.detail.clone()));
        if self.previous == failure.detail {
            self.repetitions = self.repetitions.saturating_add(1);
            if self.repetitions.is_multiple_of(20) {
                eprintln!(
                    "Dogi runtime is still {} ({} attempts): {}",
                    failure.label(),
                    self.repetitions,
                    failure.detail
                );
            }
        } else {
            eprintln!("Dogi runtime is {}: {}", failure.label(), failure.detail);
            self.previous.clone_from(&failure.detail);
            self.repetitions = 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::control::RuntimeHealthSnapshot;
    use super::*;

    #[test]
    fn restart_without_a_matching_profile_reconciles_all_base_hardware() {
        let base = base_settings();
        let state = RuntimeDeviceLeaseState::new(&base);

        let (target, plan) = state.target_plan("device", &base, false);

        assert_eq!(target, base.normalized());
        assert_complete_hardware_plan(&plan);
    }

    #[test]
    fn profile_to_session_pause_restores_base_hardware_and_native_routes() {
        let base = base_settings();
        let mut state = RuntimeDeviceLeaseState::new(&base);
        let (profile, _) = state.target_plan("device", &profile_settings(&base), false);
        state.commit(profile);

        let (target, plan) = state
            .release_plan("device")
            .expect("profile target requires restoration");

        assert_released_target(&target, &base);
        assert_complete_hardware_plan(&plan);
    }

    #[test]
    fn profile_to_service_stop_restores_the_latest_saved_base() {
        let base = base_settings();
        let mut state = RuntimeDeviceLeaseState::new(&base);
        let (profile, _) = state.target_plan("device", &profile_settings(&base), false);
        state.commit(profile);
        let updated_base = Master3sSettings {
            pointer_speed_percent: 115,
            smart_shift_threshold: 31,
            natural_scroll: true,
            ..base
        };
        state.update_base_settings(&updated_base);

        let (target, plan) = state
            .release_plan("device")
            .expect("service stop requires restoration");

        assert_released_target(&target, &updated_base);
        assert!(plan.steps.iter().any(|step| {
            matches!(
                step.operation,
                dogi_core::SettingsApplyOperation::PointerSpeed { percent: 115 }
            )
        }));
        assert!(plan.steps.iter().any(|step| {
            matches!(
                step.operation,
                dogi_core::SettingsApplyOperation::WheelBehavior { threshold: 31, .. }
            )
        }));
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
            transaction: dogi_core::SettingsTransactionState::RolledBack,
            outcomes: vec![dogi_core::SettingsApplyOutcome {
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
            dogi_core::hidpp_endpoint_id(&first.id),
            dogi_core::hidpp_endpoint_id(&second.id)
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

    fn base_settings() -> Master3sSettings {
        Master3sSettings {
            pointer_speed_percent: 85,
            smart_shift_threshold: 37,
            high_resolution_scroll: true,
            natural_scroll: false,
            ..Master3sSettings::default()
        }
        .normalized()
    }

    fn profile_settings(base: &Master3sSettings) -> Master3sSettings {
        let mut profile = Master3sSettings {
            profile_name: "Default / Editing".to_owned(),
            pointer_speed_percent: 165,
            smart_shift_enabled: false,
            smart_shift_threshold: 7,
            ratchet_mode: dogi_core::WheelRatchetMode::FreeSpin,
            high_resolution_scroll: false,
            natural_scroll: true,
            thumb_wheel: ThumbWheelMode::Zoom,
            thumb_wheel_speed_percent: 225,
            ..base.clone()
        };
        for button in Master3sButton::ALL {
            profile.set_button_action(button, ButtonAction::Action(dogi_core::Action::Copy));
        }
        profile.normalized()
    }

    fn assert_complete_hardware_plan(plan: &SettingsApplyPlan) {
        for feature in [
            HidppFeature::PointerSpeed,
            HidppFeature::SmartShift,
            HidppFeature::HiresWheel,
            HidppFeature::ThumbWheel,
        ] {
            assert_eq!(
                plan.steps
                    .iter()
                    .filter(|step| step.feature == feature)
                    .count(),
                1,
                "missing or duplicated {feature:?} step"
            );
        }
        assert_eq!(
            plan.steps
                .iter()
                .filter(|step| step.feature == HidppFeature::ReprogrammableControls)
                .count(),
            Master3sButton::ALL.len()
        );
        assert!(plan.steps.iter().all(|step| step.requires_device_write));
    }

    fn assert_released_target(target: &Master3sSettings, base: &Master3sSettings) {
        let base = base.normalized();
        assert_eq!(target.pointer_speed_percent, base.pointer_speed_percent);
        assert_eq!(target.smart_shift_enabled, base.smart_shift_enabled);
        assert_eq!(target.smart_shift_threshold, base.smart_shift_threshold);
        assert_eq!(target.ratchet_mode, base.ratchet_mode);
        assert_eq!(target.high_resolution_scroll, base.high_resolution_scroll);
        assert_eq!(target.natural_scroll, base.natural_scroll);
        assert_eq!(target.thumb_wheel, ThumbWheelMode::HorizontalScroll);
        assert_eq!(
            target.thumb_wheel_speed_percent,
            dogi_core::DEFAULT_THUMB_WHEEL_SPEED_PERCENT
        );
        assert!(
            Master3sButton::ALL
                .into_iter()
                .all(|button| target.button_action(button) == ButtonAction::Native)
        );
    }

    fn runtime_device(endpoint: &str, slot: u8, unit_id: &str) -> DeviceInfo {
        DeviceInfo {
            id: format!("{endpoint}:slot:{slot:02x}:wpid:b034"),
            name: "Logitech Bolt Receiver".to_owned(),
            paired_device: Some(dogi_core::PairedDeviceInfo {
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
            bus: dogi_core::BusKind::Usb,
            bus_id: Some(0x0003),
            vendor_id: 0x046d,
            product_id: 0xc548,
            release_number: None,
            connection: dogi_core::ConnectionKind::Bolt,
            receiver_kind: Some(dogi_core::ReceiverKind::Bolt),
            path: format!("/dev/{endpoint}"),
            sysfs_path: format!("/sys/class/hidraw/{endpoint}"),
            physical_path: Some(format!("usb-{endpoint}")),
            driver: Some("hid-generic".to_owned()),
            interface_number: Some(2),
            usage_page: Some(0xff00),
            usage: Some(0x0001),
            access: dogi_core::DeviceAccess::default(),
            battery: dogi_core::BatteryInfo::not_queried("test"),
            report_descriptor: dogi_core::ReportDescriptorInfo {
                hidpp_usage: Some(dogi_core::HidUsage {
                    usage_page: 0xff00,
                    usage: 0x0001,
                }),
                ..dogi_core::ReportDescriptorInfo::default()
            },
            capabilities: dogi_core::DeviceCapabilities::default(),
        }
    }
}
