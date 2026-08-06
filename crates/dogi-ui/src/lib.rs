use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use dogi_core::{
    AppProfile, BatteryStatus, ButtonAction, CapabilityState, ConnectionKind,
    DEFAULT_THUMB_WHEEL_SPEED_PERCENT, DeviceInfo, DogiError, MAX_THUMB_WHEEL_SPEED_PERCENT,
    MIN_THUMB_WHEEL_SPEED_PERCENT, Master3sButton, Master3sSettings, Result,
    SettingsApplyOperation, SettingsApplyPlan, SettingsApplyReport, SettingsApplyScope,
    SettingsApplyStatus, SettingsApplyStep, ThumbWheelMode, WheelRatchetMode,
    build_master3s_apply_plan, build_master3s_device_diff_plan, device_settings_id,
    known_logitech_model_name, known_logitech_product_name, known_logitech_wpid_name,
    resolved_logitech_device_name, settings_apply_step_scope,
};

slint::include_modules!();

mod desktop_preferences;
mod preferences;

pub const APPLICATION_ID: &str = "io.github.oksyd.dogi";
pub const DEVELOPMENT_APPLICATION_ID: &str = "io.github.oksyd.dogi.Development";

pub use preferences::{
    ApplicationLanguage, ApplicationPreferenceChange, ApplicationPreferenceSaver,
    ApplicationPreferences, ApplicationPreferencesIntegration, ApplicationTheme, CloseBehavior,
};

pub type SettingsLoader = Rc<dyn Fn(&str) -> Result<Master3sSettings>>;
pub type SettingsSaver = Rc<dyn Fn(Option<&str>, &Master3sSettings) -> Result<String>>;
pub type SettingsApplier =
    Rc<dyn Fn(&str, &Master3sSettings, &SettingsApplyPlan) -> Result<SettingsApplyReport>>;
pub type DeviceScanner = Arc<dyn Fn() -> Result<Vec<DeviceInfo>> + Send + Sync>;

#[derive(Clone)]
pub struct DeviceSettingsIntegration {
    pub load: SettingsLoader,
    pub save: SettingsSaver,
    pub apply: SettingsApplier,
}

#[derive(Clone)]
pub struct DeviceDiscovery {
    pub inventory: Option<DeviceScanner>,
    pub enrich: DeviceScanner,
}

impl DeviceDiscovery {
    pub fn new(inventory: DeviceScanner, enrich: DeviceScanner) -> Self {
        Self {
            inventory: Some(inventory),
            enrich,
        }
    }

    fn single(scanner: DeviceScanner) -> Self {
        Self {
            inventory: None,
            enrich: scanner,
        }
    }
}

const HIDPP_RECOVERY_SCAN_DELAYS: [Duration; 3] = [
    Duration::from_millis(700),
    Duration::from_secs(2),
    Duration::from_secs(5),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeviceScanIntent {
    User,
    HidppRecovery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeviceScanPhase {
    Inventory,
    Enriched,
}

struct DeviceScanCompletion {
    intent: DeviceScanIntent,
    phase: DeviceScanPhase,
    result: Result<Vec<DeviceInfo>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DesktopRuntimeStatus {
    pub enabled: bool,
    pub active: bool,
    pub ready: bool,
    pub app_profiles_supported: bool,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopRuntimeOperation {
    Reconcile { enabled: bool },
    Restart,
}

#[derive(Clone)]
pub struct DesktopRuntimeManager {
    pub supported: bool,
    pub availability: DesktopRuntimeAvailability,
    pub detail: String,
    pub manage: Arc<dyn Fn(DesktopRuntimeOperation) -> Result<DesktopRuntimeStatus> + Send + Sync>,
    pub horizontal_scroll_preview:
        Arc<dyn Fn(HorizontalScrollPreviewCommand) -> Result<()> + Send + Sync>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ApplicationIdentity {
    #[default]
    Stable,
    Development,
}

impl ApplicationIdentity {
    fn xdg_app_id(self) -> &'static str {
        match self {
            Self::Stable => APPLICATION_ID,
            Self::Development => DEVELOPMENT_APPLICATION_ID,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplicationUpdateCheckIntent {
    Automatic,
    UserInitiated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplicationUpdateOperation {
    Prepare(ApplicationUpdateCheckIntent),
    Install,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplicationUpdateResult {
    Deferred,
    UpToDate,
    Ready { version: String },
    Cancelled,
    Restarting,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationUpdateNotification {
    pub title: String,
    pub body: String,
}

#[derive(Clone)]
pub struct ApplicationUpdateManager {
    pub supported: bool,
    pub current_version: String,
    pub detail: String,
    pub manage:
        Arc<dyn Fn(ApplicationUpdateOperation) -> Result<ApplicationUpdateResult> + Send + Sync>,
    pub notify_ready: Arc<dyn Fn(ApplicationUpdateNotification) -> Result<()> + Send + Sync>,
}

impl ApplicationUpdateManager {
    pub fn unavailable(detail: impl Into<String>) -> Self {
        let detail = detail.into();
        let operation_detail = detail.clone();
        Self {
            supported: false,
            current_version: env!("CARGO_PKG_VERSION").to_owned(),
            detail,
            manage: Arc::new(move |_| Err(DogiError::BackendUnavailable(operation_detail.clone()))),
            notify_ready: Arc::new(|_| Ok(())),
        }
    }
}

impl Default for ApplicationUpdateManager {
    fn default() -> Self {
        Self::unavailable("Automatic updates are not configured")
    }
}

#[derive(Clone)]
pub struct UiIntegrations {
    pub identity: ApplicationIdentity,
    pub discovery: DeviceDiscovery,
    pub settings: DeviceSettingsIntegration,
    pub runtime: DesktopRuntimeManager,
    pub preferences: ApplicationPreferencesIntegration,
    pub updates: ApplicationUpdateManager,
}

#[derive(Default)]
struct LaunchIntegrations {
    identity: ApplicationIdentity,
    discovery: Option<DeviceDiscovery>,
    loader: Option<SettingsLoader>,
    saver: Option<SettingsSaver>,
    applier: Option<SettingsApplier>,
    runtime: Option<DesktopRuntimeManager>,
    preferences: ApplicationPreferencesIntegration,
    updates: ApplicationUpdateManager,
}

#[derive(Debug)]
struct DesktopRuntimeCompletion {
    result: Result<DesktopRuntimeStatus>,
}

#[derive(Debug)]
struct ApplicationUpdateCompletion {
    result: Result<ApplicationUpdateResult>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HorizontalScrollPreviewCommand {
    Set {
        device_id: String,
        speed_percent: u16,
    },
    Clear,
}

#[derive(Debug)]
struct HorizontalScrollPreviewWork {
    sequence: u64,
    command: HorizontalScrollPreviewCommand,
}

#[derive(Debug)]
struct HorizontalScrollPreviewCompletion {
    sequence: u64,
    command: HorizontalScrollPreviewCommand,
    result: Result<()>,
}

impl UiStatus {
    fn presentation(kind: UiStatusKind, message: UiMessage) -> Self {
        Self {
            kind,
            message,
            ..Self::default()
        }
    }

    fn with_subject(mut self, subject: impl Into<slint::SharedString>) -> Self {
        self.subject = subject.into();
        self
    }

    fn with_detail(mut self, detail: impl Into<slint::SharedString>) -> Self {
        self.detail = detail.into();
        self
    }

    fn with_path(mut self, path: impl Into<slint::SharedString>) -> Self {
        self.path = path.into();
        self
    }

    fn with_count(mut self, count: usize) -> Self {
        self.count = i32::try_from(count).unwrap_or(i32::MAX);
        self
    }

    fn with_apply_counts(mut self, failed: usize, applied: usize, unsupported: usize) -> Self {
        self.failed = i32::try_from(failed).unwrap_or(i32::MAX);
        self.applied = i32::try_from(applied).unwrap_or(i32::MAX);
        self.unsupported = i32::try_from(unsupported).unwrap_or(i32::MAX);
        self
    }
}

fn set_window_status(window: &MainWindow, status: UiStatus) {
    window.set_status(status);
}

fn set_desktop_runtime_status(window: &MainWindow, status: &DesktopRuntimeStatus) {
    window.set_runtime_state(if !status.enabled && !status.active {
        DesktopRuntimeState::Stopped
    } else if status.ready {
        DesktopRuntimeState::Running
    } else {
        DesktopRuntimeState::Degraded
    });
    window.set_runtime_detail(status.detail.clone().into());
    window.set_app_profiles_supported(status.app_profiles_supported);
}

fn horizontal_scroll_preview_command(
    window: &MainWindow,
    devices: &[LogicalDevice],
    session: &DeviceUiSession,
) -> Option<HorizontalScrollPreviewCommand> {
    let device = session
        .selected_index
        .and_then(|index| devices.get(index))?;
    let speed_percent = if window.get_horizontal_scroll_test_mode_index() == 0 {
        DEFAULT_THUMB_WHEEL_SPEED_PERCENT
    } else {
        (window.get_thumb_wheel_speed().round() as u16)
            .clamp(MIN_THUMB_WHEEL_SPEED_PERCENT, MAX_THUMB_WHEEL_SPEED_PERCENT)
    };
    Some(HorizontalScrollPreviewCommand::Set {
        device_id: device.primary.id.clone(),
        speed_percent,
    })
}

fn dispatch_horizontal_scroll_preview(
    sender: Option<&mpsc::Sender<HorizontalScrollPreviewWork>>,
    sequence: &Cell<u64>,
    command: HorizontalScrollPreviewCommand,
) -> bool {
    let Some(sender) = sender else {
        return false;
    };
    let next = sequence.get().wrapping_add(1).max(1);
    sequence.set(next);
    sender
        .send(HorizontalScrollPreviewWork {
            sequence: next,
            command,
        })
        .is_ok()
}

fn persist_application_setting(
    window: &MainWindow,
    save: &ApplicationPreferenceSaver,
    change: ApplicationPreferenceChange,
) -> bool {
    match save(change) {
        Ok(()) => {
            set_window_status(window, UiStatus::default());
            true
        }
        Err(error) => {
            set_window_status(
                window,
                UiStatus::presentation(UiStatusKind::Warning, UiMessage::AppConfigSaveFailed)
                    .with_detail(error.to_string()),
            );
            false
        }
    }
}

fn request_language_change(
    window: slint::Weak<MainWindow>,
    active_language: Rc<Cell<ApplicationLanguage>>,
    save: ApplicationPreferenceSaver,
    language: ApplicationLanguage,
) {
    slint::Timer::single_shot(Duration::ZERO, move || {
        let Some(window) = window.upgrade() else {
            return;
        };
        if let Err(error) = slint::select_bundled_translation(language.locale()) {
            window.set_language_index(active_language.get().index());
            set_window_status(
                &window,
                UiStatus::presentation(UiStatusKind::Error, UiMessage::LanguageUnavailable)
                    .with_detail(error.to_string()),
            );
            return;
        }

        active_language.set(language);
        window.set_language_index(language.index());
        persist_application_setting(
            &window,
            &save,
            ApplicationPreferenceChange::Language(language),
        );
    });
}

fn dispatch_desktop_runtime_operation(
    window: &MainWindow,
    sender: Option<&mpsc::Sender<DesktopRuntimeOperation>>,
    operation: DesktopRuntimeOperation,
) -> bool {
    let Some(sender) = sender else {
        return false;
    };
    window.set_runtime_busy(true);
    window.set_runtime_state(DesktopRuntimeState::Starting);
    window.set_runtime_detail("".into());
    sender.send(operation).is_ok()
}

fn dispatch_application_update(
    window: &MainWindow,
    sender: Option<&mpsc::Sender<ApplicationUpdateOperation>>,
    in_flight: &Cell<bool>,
    operation: ApplicationUpdateOperation,
) -> bool {
    if in_flight.get() {
        return true;
    }
    let Some(sender) = sender else {
        return false;
    };
    window.set_update_detail("".into());
    match operation {
        ApplicationUpdateOperation::Prepare(ApplicationUpdateCheckIntent::UserInitiated) => {
            window.set_available_version("".into());
            window.set_update_state(UpdateState::Checking);
        }
        ApplicationUpdateOperation::Prepare(ApplicationUpdateCheckIntent::Automatic) => {}
        ApplicationUpdateOperation::Install => {
            window.set_update_state(UpdateState::Installing);
        }
    }
    if sender.send(operation).is_err() {
        return false;
    }
    in_flight.set(true);
    true
}

fn run_device_discovery(
    discovery: DeviceDiscovery,
    intent: DeviceScanIntent,
    sender: &mpsc::Sender<DeviceScanCompletion>,
) {
    if intent == DeviceScanIntent::User
        && let Some(inventory) = discovery.inventory
    {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inventory()))
            .unwrap_or_else(|_| {
                Err(DogiError::Ui(
                    "device inventory scanner panicked".to_owned(),
                ))
            });
        let _ = sender.send(DeviceScanCompletion {
            intent,
            phase: DeviceScanPhase::Inventory,
            result,
        });
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (discovery.enrich)()))
        .unwrap_or_else(|_| {
            Err(DogiError::Ui(
                "device enrichment scanner panicked".to_owned(),
            ))
        });
    let _ = sender.send(DeviceScanCompletion {
        intent,
        phase: DeviceScanPhase::Enriched,
        result,
    });
}

#[derive(Clone, Debug, Default)]
pub struct UiState {
    pub devices: Vec<DeviceInfo>,
    pub settings: Master3sSettings,
}

impl UiState {
    pub fn new(devices: Vec<DeviceInfo>) -> Self {
        Self {
            devices,
            settings: Master3sSettings::default(),
        }
    }

    pub fn with_settings(devices: Vec<DeviceInfo>, settings: Master3sSettings) -> Self {
        Self { devices, settings }
    }
}

#[derive(Clone, Debug)]
struct DeviceDraft {
    transport_key: String,
    strong_key: Option<String>,
    settings: Master3sSettings,
    saved_settings: Master3sSettings,
    dirty: bool,
}

impl DeviceDraft {
    fn new(device: &LogicalDevice, settings: Master3sSettings) -> Self {
        let settings = settings.normalized();
        Self {
            transport_key: logical_device_transport_key(device),
            strong_key: logical_device_strong_key(device),
            saved_settings: settings.clone(),
            settings,
            dirty: false,
        }
    }

    fn match_rank(&self, device: &LogicalDevice) -> Option<u8> {
        let next_transport = logical_device_transport_key(device);
        let next_strong = logical_device_strong_key(device);

        if self.strong_key.is_some() && self.strong_key == next_strong {
            return Some(0);
        }

        if self.transport_key != next_transport {
            return None;
        }

        match (&self.strong_key, next_strong.as_ref()) {
            (Some(current), Some(next)) if current != next => None,
            _ => Some(1),
        }
    }

    fn carried_to(&self, device: &LogicalDevice) -> Self {
        Self {
            transport_key: logical_device_transport_key(device),
            strong_key: logical_device_strong_key(device).or_else(|| self.strong_key.clone()),
            settings: self.settings.clone(),
            saved_settings: self.saved_settings.clone(),
            dirty: self.dirty,
        }
    }
}

#[derive(Clone, Debug)]
struct DetachedDraft {
    settings: Master3sSettings,
    saved_settings: Master3sSettings,
    dirty: bool,
}

#[derive(Clone, Debug)]
struct DeviceUiSession {
    selected_index: Option<usize>,
    drafts: Vec<DeviceDraft>,
    detached_drafts: HashMap<String, DetachedDraft>,
    fallback: Master3sSettings,
    fallback_saved: Master3sSettings,
    fallback_dirty: bool,
}

impl DeviceUiSession {
    fn new(
        devices: &[LogicalDevice],
        settings: Vec<Master3sSettings>,
        fallback: Master3sSettings,
    ) -> Self {
        let drafts = devices
            .iter()
            .zip(settings)
            .map(|(device, settings)| DeviceDraft::new(device, settings))
            .collect::<Vec<_>>();
        let fallback = fallback.normalized();
        Self {
            selected_index: (!drafts.is_empty()).then_some(0),
            drafts,
            detached_drafts: HashMap::new(),
            fallback_saved: fallback.clone(),
            fallback,
            fallback_dirty: false,
        }
    }

    fn current(&self) -> &Master3sSettings {
        self.selected_index
            .and_then(|index| self.drafts.get(index))
            .map(|draft| &draft.settings)
            .unwrap_or(&self.fallback)
    }

    fn current_saved(&self) -> &Master3sSettings {
        self.selected_index
            .and_then(|index| self.drafts.get(index))
            .map(|draft| &draft.saved_settings)
            .unwrap_or(&self.fallback_saved)
    }

    fn capture_window(&mut self, window: &MainWindow) -> Master3sSettings {
        let settings = settings_from_window(window, self.current()).normalized();
        self.replace_current(settings);
        self.current().clone()
    }

    fn current_dirty(&self) -> bool {
        self.selected_index
            .and_then(|index| self.drafts.get(index))
            .map(|draft| draft.dirty)
            .unwrap_or(self.fallback_dirty)
    }

    fn any_dirty(&self) -> bool {
        self.fallback_dirty
            || self.drafts.iter().any(|draft| draft.dirty)
            || self.detached_drafts.values().any(|draft| draft.dirty)
    }

    fn dirty_settings(&self, devices: &[LogicalDevice]) -> Vec<(Option<String>, Master3sSettings)> {
        let mut settings = Vec::new();
        if self.fallback_dirty {
            settings.push((None, self.fallback.clone()));
        }

        let mut connected_keys = HashSet::new();
        for (index, draft) in self.drafts.iter().enumerate() {
            let settings_id = draft.strong_key.clone().or_else(|| {
                devices
                    .get(index)
                    .map(|device| device_settings_id(&device.primary))
            });
            if let Some(key) = &settings_id {
                connected_keys.insert(key.clone());
            }
            if draft.dirty {
                settings.push((settings_id, draft.settings.clone()));
            }
        }

        settings.extend(
            self.detached_drafts
                .iter()
                .filter(|(key, draft)| draft.dirty && !connected_keys.contains(*key))
                .map(|(key, draft)| (Some(key.clone()), draft.settings.clone())),
        );
        settings
    }

    fn replace_current(&mut self, settings: Master3sSettings) {
        let settings = settings.normalized();
        if let Some(draft) = self
            .selected_index
            .and_then(|index| self.drafts.get_mut(index))
        {
            draft.dirty = settings != draft.saved_settings;
            draft.settings = settings;
        } else {
            self.fallback_dirty = settings != self.fallback_saved;
            self.fallback = settings;
        }
    }

    fn mark_current_saved(&mut self) {
        if let Some(draft) = self
            .selected_index
            .and_then(|index| self.drafts.get_mut(index))
        {
            draft.saved_settings = draft.settings.clone();
            draft.dirty = false;
        } else {
            self.fallback_saved = self.fallback.clone();
            self.fallback_dirty = false;
        }
    }

    fn mark_all_saved(&mut self) {
        self.fallback_saved = self.fallback.clone();
        self.fallback_dirty = false;
        for draft in &mut self.drafts {
            draft.saved_settings = draft.settings.clone();
            draft.dirty = false;
            if let Some(strong_key) = &draft.strong_key
                && let Some(detached) = self.detached_drafts.get_mut(strong_key)
            {
                detached.settings = draft.settings.clone();
                detached.saved_settings = draft.settings.clone();
                detached.dirty = false;
            }
        }
        for draft in self.detached_drafts.values_mut() {
            draft.saved_settings = draft.settings.clone();
            draft.dirty = false;
        }
    }

    fn revert_current(&mut self) {
        if let Some(draft) = self
            .selected_index
            .and_then(|index| self.drafts.get_mut(index))
        {
            draft.settings = draft.saved_settings.clone();
            draft.dirty = false;
        } else {
            self.fallback = self.fallback_saved.clone();
            self.fallback_dirty = false;
        }
    }

    fn select(&mut self, index: usize) -> bool {
        if index >= self.drafts.len() {
            return false;
        }
        self.selected_index = Some(index);
        true
    }

    fn reconcile_devices(
        &mut self,
        next_devices: &[LogicalDevice],
        loader: Option<&SettingsLoader>,
    ) -> Result<()> {
        let selected_draft = self
            .selected_index
            .and_then(|index| self.drafts.get(index))
            .cloned();
        let current_drafts = self.drafts.clone();
        let mut detached_drafts = self.detached_drafts.clone();

        for draft in &current_drafts {
            if let Some(strong_key) = &draft.strong_key {
                detached_drafts.insert(
                    strong_key.clone(),
                    DetachedDraft {
                        settings: draft.settings.clone(),
                        saved_settings: draft.saved_settings.clone(),
                        dirty: draft.dirty,
                    },
                );
            }
        }

        let mut consumed = vec![false; current_drafts.len()];
        let mut next_drafts = Vec::with_capacity(next_devices.len());
        for device in next_devices {
            let current_match = current_drafts
                .iter()
                .enumerate()
                .filter(|(index, _)| !consumed[*index])
                .filter_map(|(index, draft)| draft.match_rank(device).map(|rank| (rank, index)))
                .min()
                .map(|(_, index)| index);

            if let Some(index) = current_match {
                consumed[index] = true;
                next_drafts.push(current_drafts[index].carried_to(device));
                continue;
            }

            if let Some(strong_key) = logical_device_strong_key(device)
                && let Some(detached) = detached_drafts.get(&strong_key)
            {
                let mut draft = DeviceDraft::new(device, detached.settings.clone());
                draft.saved_settings = detached.saved_settings.clone();
                draft.dirty = detached.dirty;
                next_drafts.push(draft);
                continue;
            }

            let settings = match loader {
                Some(loader) => loader(&device_settings_id(&device.primary))?.normalized(),
                None => self.fallback.clone(),
            };
            next_drafts.push(DeviceDraft::new(device, settings));
        }

        self.selected_index = selected_draft
            .and_then(|selected| {
                next_drafts
                    .iter()
                    .zip(next_devices)
                    .position(|(draft, device)| {
                        selected.match_rank(device).is_some()
                            && draft.strong_key == selected.strong_key
                    })
                    .or_else(|| {
                        next_devices
                            .iter()
                            .position(|device| selected.match_rank(device).is_some())
                    })
            })
            .or_else(|| (!next_devices.is_empty()).then_some(0));
        self.drafts = next_drafts;
        self.detached_drafts = detached_drafts;
        Ok(())
    }
}

pub fn launch(state: UiState) -> Result<()> {
    launch_internal(state, LaunchIntegrations::default())
}

pub fn launch_with_settings_saver(state: UiState, saver: SettingsSaver) -> Result<()> {
    launch_internal(
        state,
        LaunchIntegrations {
            saver: Some(saver),
            ..LaunchIntegrations::default()
        },
    )
}

pub fn launch_with_settings_io(
    state: UiState,
    loader: SettingsLoader,
    saver: SettingsSaver,
    applier: SettingsApplier,
) -> Result<()> {
    launch_internal(
        state,
        LaunchIntegrations {
            loader: Some(loader),
            saver: Some(saver),
            applier: Some(applier),
            ..LaunchIntegrations::default()
        },
    )
}

pub fn launch_with_device_io(
    state: UiState,
    scanner: DeviceScanner,
    loader: SettingsLoader,
    saver: SettingsSaver,
    applier: SettingsApplier,
) -> Result<()> {
    launch_internal(
        state,
        LaunchIntegrations {
            discovery: Some(DeviceDiscovery::single(scanner)),
            loader: Some(loader),
            saver: Some(saver),
            applier: Some(applier),
            ..LaunchIntegrations::default()
        },
    )
}

pub fn launch_with_integrations(state: UiState, integrations: UiIntegrations) -> Result<()> {
    let UiIntegrations {
        identity,
        discovery,
        settings,
        runtime,
        preferences,
        updates,
    } = integrations;
    launch_internal(
        state,
        LaunchIntegrations {
            identity,
            discovery: Some(discovery),
            loader: Some(settings.load),
            saver: Some(settings.save),
            applier: Some(settings.apply),
            runtime: Some(runtime),
            preferences,
            updates,
        },
    )
}

fn launch_internal(state: UiState, integrations: LaunchIntegrations) -> Result<()> {
    let LaunchIntegrations {
        identity,
        discovery,
        loader,
        saver,
        applier,
        runtime,
        preferences,
        updates,
    } = integrations;
    let window = MainWindow::new().map_err(|error| DogiError::Ui(error.to_string()))?;
    slint::set_xdg_app_id(identity.xdg_app_id())
        .map_err(|error| DogiError::Ui(error.to_string()))?;
    let app_preferences = preferences.initial;
    let mut startup_status = preferences
        .load_error
        .map_or_else(UiStatus::default, |error| {
            UiStatus::presentation(UiStatusKind::Warning, UiMessage::AppConfigLoadFailed)
                .with_detail(error)
        });
    let language = app_preferences.language;
    let theme = app_preferences.theme;
    let mut close_behavior = app_preferences.close_behavior;
    let background_operations_enabled = app_preferences.background_operations_enabled;
    let automatic_update_checks_enabled = app_preferences.automatic_update_checks_enabled;
    if let Err(error) = slint::select_bundled_translation(language.locale()) {
        startup_status =
            UiStatus::presentation(UiStatusKind::Warning, UiMessage::LanguageUnavailable)
                .with_detail(error.to_string());
    }
    window.set_language_index(language.index());
    window.set_theme_index(theme.index());
    desktop_preferences::watch_high_contrast(window.as_weak());

    let tray = match AppTray::new() {
        Ok(tray) => Some(tray),
        Err(error) => {
            if close_behavior == CloseBehavior::MinimizeToTray {
                close_behavior = CloseBehavior::Quit;
                startup_status =
                    UiStatus::presentation(UiStatusKind::Warning, UiMessage::TrayUnavailable)
                        .with_detail(error.to_string());
            }
            None
        }
    };
    window.set_close_behavior_index(close_behavior.index());
    let active_close_behavior = Rc::new(Cell::new(close_behavior));
    if let Some(tray) = &tray {
        tray.set_enabled(close_behavior == CloseBehavior::MinimizeToTray);

        let open_window = window.as_weak();
        tray.on_open_window(move || {
            if let Some(window) = open_window.upgrade() {
                let _ = window.show();
            }
        });

        let open_updates_window = window.as_weak();
        tray.on_open_updates(move || {
            if let Some(window) = open_updates_window.upgrade() {
                window.set_confirm_visible(false);
                window.set_page_index(3);
                let _ = window.show();
            }
        });
    }

    let logical_devices = Rc::new(RefCell::new(logical_devices(&state.devices)));
    let rows = logical_devices
        .borrow()
        .iter()
        .map(device_row_from_logical)
        .collect::<Vec<_>>();
    let fallback_settings = state.settings.normalized();
    let drafts = logical_devices
        .borrow()
        .iter()
        .map(|device| match &loader {
            Some(loader) => {
                loader(&device_settings_id(&device.primary)).map(|settings| settings.normalized())
            }
            None => Ok(fallback_settings.clone()),
        })
        .collect::<Result<Vec<_>>>()?;
    let session = Rc::new(RefCell::new(DeviceUiSession::new(
        &logical_devices.borrow(),
        drafts,
        fallback_settings,
    )));
    let runtime_supported = runtime.as_ref().is_some_and(|runtime| runtime.supported);
    let runtime_detail = runtime
        .as_ref()
        .map(|runtime| runtime.detail.clone())
        .unwrap_or_default();
    let preview_handler = runtime
        .as_ref()
        .map(|runtime| runtime.horizontal_scroll_preview.clone());
    let runtime_handler = runtime
        .as_ref()
        .filter(|runtime| runtime.supported)
        .map(|runtime| runtime.manage.clone());
    let (preview_work_sender, preview_work_receiver) =
        mpsc::channel::<HorizontalScrollPreviewWork>();
    let (preview_completion_sender, preview_completion_receiver) =
        mpsc::channel::<HorizontalScrollPreviewCompletion>();
    let preview_worker_available = preview_handler.is_some_and(|handler| {
        std::thread::Builder::new()
            .name("dogi-horizontal-scroll-preview".to_owned())
            .spawn(move || {
                while let Ok(work) = preview_work_receiver.recv() {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        handler(work.command.clone())
                    }))
                    .unwrap_or_else(|_| {
                        Err(DogiError::Ui(
                            "horizontal scroll preview worker panicked".to_owned(),
                        ))
                    });
                    let _ = preview_completion_sender.send(HorizontalScrollPreviewCompletion {
                        sequence: work.sequence,
                        command: work.command,
                        result,
                    });
                }
            })
            .is_ok()
    });
    let preview_work_sender = preview_worker_available.then_some(preview_work_sender);
    let preview_sequence = Rc::new(Cell::new(0_u64));
    window.set_horizontal_scroll_test_supported(preview_worker_available);

    let confirm_quit_window = window.as_weak();
    let confirm_quit_tray = tray.as_ref().map(|tray| tray.as_weak());
    window.on_confirm_quit(move || {
        if let Some(tray) = confirm_quit_tray.as_ref().and_then(|tray| tray.upgrade()) {
            tray.set_enabled(false);
        }
        if let Some(window) = confirm_quit_window.upgrade() {
            if window.get_horizontal_scroll_test_open() {
                window.set_horizontal_scroll_test_open(false);
                window.invoke_horizontal_scroll_test_toggled(false);
            }
            window.set_quit_confirm_visible(false);
            let _ = window.hide();
        }
        let _ = slint::quit_event_loop();
    });

    let cancel_quit_window = window.as_weak();
    window.on_cancel_quit(move || {
        if let Some(window) = cancel_quit_window.upgrade() {
            window.set_quit_confirm_visible(false);
        }
    });

    if let Some(tray) = &tray {
        let tray_quit_window = window.as_weak();
        let tray_quit_session = session.clone();
        let tray_quit_icon = tray.as_weak();
        tray.on_quit_app(move || {
            let Some(window) = tray_quit_window.upgrade() else {
                return;
            };
            if tray_quit_session.borrow().any_dirty() {
                let _ = window.show();
                window.set_quit_confirm_visible(true);
                return;
            }
            if let Some(tray) = tray_quit_icon.upgrade() {
                tray.set_enabled(false);
            }
            if window.get_horizontal_scroll_test_open() {
                window.set_horizontal_scroll_test_open(false);
                window.invoke_horizontal_scroll_test_toggled(false);
            }
            let _ = window.hide();
            let _ = slint::quit_event_loop();
        });
    }

    let close_request_window = window.as_weak();
    let close_request_session = session.clone();
    let close_request_behavior = active_close_behavior.clone();
    window.window().on_close_requested(move || {
        if let Some(window) = close_request_window.upgrade()
            && window.get_horizontal_scroll_test_open()
        {
            window.set_horizontal_scroll_test_open(false);
            window.invoke_horizontal_scroll_test_toggled(false);
        }
        if close_request_behavior.get() == CloseBehavior::MinimizeToTray {
            return slint::CloseRequestResponse::HideWindow;
        }
        let Some(window) = close_request_window.upgrade() else {
            return slint::CloseRequestResponse::HideWindow;
        };
        if close_request_session.borrow().any_dirty() {
            window.set_quit_confirm_visible(true);
            slint::CloseRequestResponse::KeepWindowShown
        } else {
            let _ = slint::quit_event_loop();
            slint::CloseRequestResponse::HideWindow
        }
    });

    window.set_devices(Rc::new(slint::VecModel::from(rows)).into());
    window.set_confirm_visible(false);
    window.set_confirm_change_count(0);
    window.set_rescan_enabled(discovery.is_some());
    window.set_rescan_in_progress(false);
    refresh_selected_device_view(&window, &logical_devices.borrow(), &session.borrow());
    window.set_selected_button_index(2);
    set_window_status(&window, startup_status);

    window.set_runtime_enabled(background_operations_enabled && runtime_supported);
    window.set_runtime_busy(false);
    window.set_runtime_management_supported(runtime_supported);
    window.set_runtime_state(if runtime_supported {
        DesktopRuntimeState::Starting
    } else {
        DesktopRuntimeState::Stopped
    });
    window.set_runtime_detail(runtime_detail.into());
    window.set_runtime_availability(
        runtime
            .as_ref()
            .map(|runtime| runtime.availability)
            .unwrap_or(DesktopRuntimeAvailability::Unmanaged),
    );
    window.set_app_profiles_supported(!runtime_supported);

    let (runtime_work_sender, runtime_work_receiver) = mpsc::channel::<DesktopRuntimeOperation>();
    let (runtime_completion_sender, runtime_completion_receiver) =
        mpsc::channel::<DesktopRuntimeCompletion>();
    let runtime_worker_available = runtime_handler.is_some_and(|handler| {
        std::thread::Builder::new()
            .name("dogi-runtime-manager".to_owned())
            .spawn(move || {
                while let Ok(operation) = runtime_work_receiver.recv() {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        handler(operation)
                    }))
                    .unwrap_or_else(|_| {
                        Err(DogiError::Ui(
                            "Dogi desktop runtime manager panicked".to_owned(),
                        ))
                    });
                    let _ = runtime_completion_sender.send(DesktopRuntimeCompletion { result });
                }
            })
            .is_ok()
    });
    let runtime_work_sender = runtime_worker_available.then_some(runtime_work_sender);
    window.set_runtime_management_supported(runtime_worker_available);

    let runtime_timer = Rc::new(slint::Timer::default());
    let runtime_timer_control = Rc::downgrade(&runtime_timer);
    let runtime_timer_window = window.as_weak();
    runtime_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(40),
        move || {
            let completion = match runtime_completion_receiver.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if let Some(timer) = runtime_timer_control.upgrade() {
                        timer.stop();
                    }
                    return;
                }
            };
            if let Some(timer) = runtime_timer_control.upgrade() {
                timer.stop();
            }
            let Some(window) = runtime_timer_window.upgrade() else {
                return;
            };
            window.set_runtime_busy(false);
            match completion.result {
                Ok(status) => set_desktop_runtime_status(&window, &status),
                Err(error) => {
                    window.set_runtime_state(DesktopRuntimeState::Degraded);
                    window.set_runtime_detail(error.to_string().into());
                }
            }
        },
    );
    runtime_timer.stop();

    let runtime_startup_pending = Rc::new(Cell::new(runtime_worker_available));
    if runtime_worker_available && (discovery.is_none() || !logical_devices.borrow().is_empty()) {
        runtime_startup_pending.set(false);
        runtime_timer.restart();
        if !dispatch_desktop_runtime_operation(
            &window,
            runtime_work_sender.as_ref(),
            DesktopRuntimeOperation::Reconcile {
                enabled: background_operations_enabled,
            },
        ) {
            runtime_timer.stop();
            window.set_runtime_busy(false);
            window.set_runtime_state(DesktopRuntimeState::Degraded);
            window.set_runtime_detail("Dogi runtime manager is unavailable".into());
        }
    }

    let preview_poll_timer = Rc::new(slint::Timer::default());
    let preview_poll_control = Rc::downgrade(&preview_poll_timer);
    let preview_poll_window = window.as_weak();
    let preview_poll_sequence = preview_sequence.clone();
    preview_poll_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(40),
        move || loop {
            let completion = match preview_completion_receiver.try_recv() {
                Ok(completion) => completion,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if let Some(timer) = preview_poll_control.upgrade() {
                        timer.stop();
                    }
                    if let Some(window) = preview_poll_window.upgrade() {
                        window.set_horizontal_scroll_test_active(false);
                        window.set_horizontal_scroll_test_busy(false);
                        window.set_horizontal_scroll_test_detail(
                            "Horizontal scroll preview stopped unexpectedly".into(),
                        );
                    }
                    return;
                }
            };
            if completion.sequence != preview_poll_sequence.get() {
                continue;
            }
            let Some(window) = preview_poll_window.upgrade() else {
                return;
            };
            window.set_horizontal_scroll_test_busy(false);
            match (completion.command, completion.result) {
                (HorizontalScrollPreviewCommand::Set { .. }, Ok(())) => {
                    window.set_horizontal_scroll_test_active(true);
                    window.set_horizontal_scroll_test_detail("".into());
                }
                (HorizontalScrollPreviewCommand::Clear, Ok(())) => {
                    window.set_horizontal_scroll_test_active(false);
                    window.set_horizontal_scroll_test_detail("".into());
                }
                (_, Err(error)) => {
                    window.set_horizontal_scroll_test_active(false);
                    window.set_horizontal_scroll_test_detail(error.to_string().into());
                }
            }
        },
    );
    if !preview_worker_available {
        preview_poll_timer.stop();
    }

    let preview_toggle_sender = preview_work_sender.clone();
    let preview_toggle_sequence = preview_sequence.clone();
    let preview_toggle_devices = logical_devices.clone();
    let preview_toggle_session = session.clone();
    let preview_toggle_window = window.as_weak();
    window.on_horizontal_scroll_test_toggled(move |open| {
        let Some(window) = preview_toggle_window.upgrade() else {
            return;
        };
        window.set_horizontal_scroll_test_detail("".into());
        if !open {
            window.set_horizontal_scroll_test_active(false);
            window.set_horizontal_scroll_test_busy(false);
            let _ = dispatch_horizontal_scroll_preview(
                preview_toggle_sender.as_ref(),
                &preview_toggle_sequence,
                HorizontalScrollPreviewCommand::Clear,
            );
            return;
        }

        let command = horizontal_scroll_preview_command(
            &window,
            &preview_toggle_devices.borrow(),
            &preview_toggle_session.borrow(),
        );
        let Some(command) = command else {
            window.set_horizontal_scroll_test_open(false);
            window.set_horizontal_scroll_test_detail("No active mouse is available".into());
            return;
        };
        window.set_horizontal_scroll_test_active(false);
        window.set_horizontal_scroll_test_busy(true);
        if !dispatch_horizontal_scroll_preview(
            preview_toggle_sender.as_ref(),
            &preview_toggle_sequence,
            command,
        ) {
            window.set_horizontal_scroll_test_busy(false);
            window.set_horizontal_scroll_test_detail(
                "Horizontal scroll preview is unavailable".into(),
            );
        }
    });

    let preview_mode_sender = preview_work_sender.clone();
    let preview_mode_sequence = preview_sequence.clone();
    let preview_mode_devices = logical_devices.clone();
    let preview_mode_session = session.clone();
    let preview_mode_window = window.as_weak();
    window.on_horizontal_scroll_test_mode_selected(move |_index| {
        let Some(window) = preview_mode_window.upgrade() else {
            return;
        };
        if !window.get_horizontal_scroll_test_open() {
            return;
        }
        let Some(command) = horizontal_scroll_preview_command(
            &window,
            &preview_mode_devices.borrow(),
            &preview_mode_session.borrow(),
        ) else {
            return;
        };
        window.set_horizontal_scroll_test_active(false);
        window.set_horizontal_scroll_test_busy(true);
        window.set_horizontal_scroll_test_detail("".into());
        if !dispatch_horizontal_scroll_preview(
            preview_mode_sender.as_ref(),
            &preview_mode_sequence,
            command,
        ) {
            window.set_horizontal_scroll_test_busy(false);
            window.set_horizontal_scroll_test_detail(
                "Horizontal scroll preview is unavailable".into(),
            );
        }
    });

    let preview_speed_timer = Rc::new(slint::Timer::default());
    let preview_speed_sender = preview_work_sender.clone();
    let preview_speed_sequence = preview_sequence.clone();
    let preview_speed_devices = logical_devices.clone();
    let preview_speed_session = session.clone();
    let preview_speed_window = window.as_weak();
    preview_speed_timer.start(
        slint::TimerMode::SingleShot,
        Duration::from_millis(140),
        move || {
            let Some(window) = preview_speed_window.upgrade() else {
                return;
            };
            if !window.get_horizontal_scroll_test_open()
                || window.get_horizontal_scroll_test_mode_index() != 1
            {
                return;
            }
            let Some(command) = horizontal_scroll_preview_command(
                &window,
                &preview_speed_devices.borrow(),
                &preview_speed_session.borrow(),
            ) else {
                return;
            };
            window.set_horizontal_scroll_test_active(false);
            window.set_horizontal_scroll_test_busy(true);
            window.set_horizontal_scroll_test_detail("".into());
            if !dispatch_horizontal_scroll_preview(
                preview_speed_sender.as_ref(),
                &preview_speed_sequence,
                command,
            ) {
                window.set_horizontal_scroll_test_busy(false);
                window.set_horizontal_scroll_test_detail(
                    "Horizontal scroll preview is unavailable".into(),
                );
            }
        },
    );
    preview_speed_timer.stop();
    let preview_speed_restart = preview_speed_timer.clone();
    let preview_speed_edit_window = window.as_weak();
    window.on_horizontal_scroll_speed_edited(move || {
        let Some(window) = preview_speed_edit_window.upgrade() else {
            return;
        };
        if window.get_horizontal_scroll_test_open()
            && window.get_horizontal_scroll_test_mode_index() == 1
        {
            preview_speed_restart.restart();
        }
    });

    let preview_heartbeat_timer = Rc::new(slint::Timer::default());
    let preview_heartbeat_sender = preview_work_sender.clone();
    let preview_heartbeat_sequence = preview_sequence.clone();
    let preview_heartbeat_devices = logical_devices.clone();
    let preview_heartbeat_session = session.clone();
    let preview_heartbeat_window = window.as_weak();
    preview_heartbeat_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(3),
        move || {
            let Some(window) = preview_heartbeat_window.upgrade() else {
                return;
            };
            if !window.get_horizontal_scroll_test_open()
                || !window.get_horizontal_scroll_test_active()
                || window.get_horizontal_scroll_test_busy()
            {
                return;
            }
            let Some(command) = horizontal_scroll_preview_command(
                &window,
                &preview_heartbeat_devices.borrow(),
                &preview_heartbeat_session.borrow(),
            ) else {
                return;
            };
            let _ = dispatch_horizontal_scroll_preview(
                preview_heartbeat_sender.as_ref(),
                &preview_heartbeat_sequence,
                command,
            );
        },
    );
    if !preview_worker_available {
        preview_heartbeat_timer.stop();
    }

    let active_background_operations = Rc::new(Cell::new(background_operations_enabled));
    let background_save = preferences.save.clone();
    let background_window = window.as_weak();
    let background_sender = runtime_work_sender.clone();
    let background_timer = runtime_timer.clone();
    let background_startup_pending = runtime_startup_pending.clone();
    let selected_background_operations = active_background_operations.clone();
    window.on_runtime_enabled_changed(move |enabled| {
        let Some(window) = background_window.upgrade() else {
            return;
        };
        if !persist_application_setting(
            &window,
            &background_save,
            ApplicationPreferenceChange::BackgroundOperationsEnabled(enabled),
        ) {
            window.set_runtime_enabled(selected_background_operations.get());
            return;
        }

        selected_background_operations.set(enabled);
        background_startup_pending.set(false);
        window.set_runtime_enabled(enabled);
        background_timer.restart();
        if !dispatch_desktop_runtime_operation(
            &window,
            background_sender.as_ref(),
            DesktopRuntimeOperation::Reconcile { enabled },
        ) {
            background_timer.stop();
            window.set_runtime_busy(false);
            window.set_runtime_state(DesktopRuntimeState::Degraded);
            window.set_runtime_detail("Dogi runtime manager is unavailable".into());
        }
    });

    let restart_window = window.as_weak();
    let restart_sender = runtime_work_sender.clone();
    let restart_timer = runtime_timer.clone();
    let restart_startup_pending = runtime_startup_pending.clone();
    window.on_restart_runtime(move || {
        let Some(window) = restart_window.upgrade() else {
            return;
        };
        let operation = if window.get_runtime_enabled() {
            DesktopRuntimeOperation::Restart
        } else {
            DesktopRuntimeOperation::Reconcile { enabled: false }
        };
        restart_startup_pending.set(false);
        restart_timer.restart();
        if !dispatch_desktop_runtime_operation(&window, restart_sender.as_ref(), operation) {
            restart_timer.stop();
            window.set_runtime_busy(false);
            window.set_runtime_state(DesktopRuntimeState::Degraded);
            window.set_runtime_detail("Dogi runtime manager is unavailable".into());
        }
    });

    window.set_current_version(updates.current_version.clone().into());
    window.set_available_version("".into());
    window.set_automatic_update_checks_enabled(automatic_update_checks_enabled);

    let (update_work_sender, update_work_receiver) = mpsc::channel::<ApplicationUpdateOperation>();
    let (update_completion_sender, update_completion_receiver) =
        mpsc::channel::<ApplicationUpdateCompletion>();
    let update_handler = updates.supported.then(|| updates.manage.clone());
    let update_worker_available = update_handler.is_some_and(|handler| {
        std::thread::Builder::new()
            .name("dogi-update-manager".to_owned())
            .spawn(move || {
                while let Ok(operation) = update_work_receiver.recv() {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        handler(operation)
                    }))
                    .unwrap_or_else(|_| {
                        Err(DogiError::Ui(
                            "Dogi update manager panicked unexpectedly".to_owned(),
                        ))
                    });
                    let _ = update_completion_sender.send(ApplicationUpdateCompletion { result });
                }
            })
            .is_ok()
    });
    let update_work_sender = update_worker_available.then_some(update_work_sender);
    window.set_update_supported(update_worker_available);
    window.set_update_state(if update_worker_available {
        UpdateState::Idle
    } else {
        UpdateState::Unavailable
    });
    window.set_update_detail(if update_worker_available {
        "".into()
    } else if updates.detail.is_empty() {
        "Automatic update worker is unavailable".into()
    } else {
        updates.detail.clone().into()
    });

    let update_in_flight = Rc::new(Cell::new(false));
    let update_poll_timer = Rc::new(slint::Timer::default());
    let update_poll_control = Rc::downgrade(&update_poll_timer);
    let update_poll_window = window.as_weak();
    let update_poll_in_flight = update_in_flight.clone();
    let update_poll_tray = tray.as_ref().map(|tray| tray.as_weak());
    let update_notifier = updates.notify_ready.clone();
    let notified_update_version = Rc::new(RefCell::new(None::<String>));
    let update_poll_notified_version = notified_update_version.clone();
    update_poll_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(40),
        move || {
            let completion = match update_completion_receiver.try_recv() {
                Ok(completion) => completion,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    update_poll_in_flight.set(false);
                    if let Some(timer) = update_poll_control.upgrade() {
                        timer.stop();
                    }
                    if let Some(window) = update_poll_window.upgrade() {
                        window.set_update_state(UpdateState::Failed);
                        window.set_update_detail("Dogi update manager stopped unexpectedly".into());
                    }
                    return;
                }
            };
            update_poll_in_flight.set(false);
            if let Some(timer) = update_poll_control.upgrade() {
                timer.stop();
            }
            let Some(window) = update_poll_window.upgrade() else {
                return;
            };
            match completion.result {
                Ok(ApplicationUpdateResult::Deferred) => {
                    if window.get_update_state() == UpdateState::Checking {
                        window.set_update_state(UpdateState::Idle);
                    }
                }
                Ok(ApplicationUpdateResult::UpToDate) => {
                    window.set_available_version("".into());
                    window.set_update_detail("".into());
                    window.set_update_state(UpdateState::Current);
                    if let Some(tray) = update_poll_tray.as_ref().and_then(|tray| tray.upgrade()) {
                        tray.set_update_ready_version("".into());
                    }
                }
                Ok(ApplicationUpdateResult::Ready { version }) => {
                    let is_new = update_poll_notified_version
                        .borrow()
                        .as_deref()
                        .is_none_or(|notified| notified != version);
                    window.set_available_version(version.clone().into());
                    window.set_update_detail("".into());
                    window.set_update_state(UpdateState::Ready);
                    if let Some(tray) = update_poll_tray.as_ref().and_then(|tray| tray.upgrade()) {
                        tray.set_update_ready_version(version.clone().into());
                    }
                    if is_new && !window.window().is_visible() {
                        let notifier = update_notifier.clone();
                        let notification = ApplicationUpdateNotification {
                            title: window.get_update_notification_title().to_string(),
                            body: window.get_update_notification_body().to_string(),
                        };
                        let _ = std::thread::Builder::new()
                            .name("dogi-update-notification".to_owned())
                            .spawn(move || {
                                let _ = notifier(notification);
                            });
                    }
                    update_poll_notified_version.borrow_mut().replace(version);
                }
                Ok(ApplicationUpdateResult::Cancelled) => {
                    window.set_update_detail("".into());
                    window.set_update_state(UpdateState::Ready);
                }
                Ok(ApplicationUpdateResult::Restarting) => {
                    if let Some(tray) = update_poll_tray.as_ref().and_then(|tray| tray.upgrade()) {
                        tray.set_enabled(false);
                    }
                    let _ = window.hide();
                    let _ = slint::quit_event_loop();
                }
                Err(error) => {
                    window.set_update_detail(error.to_string().into());
                    window.set_update_state(UpdateState::Failed);
                }
            }
        },
    );
    update_poll_timer.stop();

    let check_update_window = window.as_weak();
    let check_update_sender = update_work_sender.clone();
    let check_update_in_flight = update_in_flight.clone();
    let check_update_timer = update_poll_timer.clone();
    window.on_check_for_updates(move || {
        let Some(window) = check_update_window.upgrade() else {
            return;
        };
        if !window.get_update_supported() || check_update_in_flight.get() {
            return;
        }
        if dispatch_application_update(
            &window,
            check_update_sender.as_ref(),
            &check_update_in_flight,
            ApplicationUpdateOperation::Prepare(ApplicationUpdateCheckIntent::UserInitiated),
        ) {
            check_update_timer.restart();
        } else {
            window.set_update_state(UpdateState::Failed);
            window.set_update_detail("Dogi update manager is unavailable".into());
        }
    });

    let automatic_check_window = window.as_weak();
    let automatic_check_sender = update_work_sender.clone();
    let automatic_check_in_flight = update_in_flight.clone();
    let automatic_check_timer = update_poll_timer.clone();
    window.on_automatic_update_check(move || {
        let Some(window) = automatic_check_window.upgrade() else {
            return;
        };
        if !window.get_update_supported()
            || automatic_check_in_flight.get()
            || !window.get_available_version().is_empty()
        {
            return;
        }
        if dispatch_application_update(
            &window,
            automatic_check_sender.as_ref(),
            &automatic_check_in_flight,
            ApplicationUpdateOperation::Prepare(ApplicationUpdateCheckIntent::Automatic),
        ) {
            automatic_check_timer.restart();
        }
    });

    let install_update_window = window.as_weak();
    let install_update_sender = update_work_sender.clone();
    let install_update_in_flight = update_in_flight.clone();
    let install_update_timer = update_poll_timer.clone();
    let install_update = Rc::new(move || {
        let Some(window) = install_update_window.upgrade() else {
            return;
        };
        if !window.get_update_supported()
            || install_update_in_flight.get()
            || window.get_available_version().is_empty()
        {
            return;
        }
        if dispatch_application_update(
            &window,
            install_update_sender.as_ref(),
            &install_update_in_flight,
            ApplicationUpdateOperation::Install,
        ) {
            install_update_timer.restart();
        } else {
            window.set_update_state(UpdateState::Failed);
            window.set_update_detail("Dogi update manager is unavailable".into());
        }
    });

    let install_request_window = window.as_weak();
    let install_request_session = session.clone();
    let install_request_action = install_update.clone();
    window.on_install_update_requested(move || {
        let Some(window) = install_request_window.upgrade() else {
            return;
        };
        {
            let mut session = install_request_session.borrow_mut();
            session.capture_window(&window);
            window.set_draft_dirty(session.current_dirty());
            if session.any_dirty() {
                window.set_update_install_confirm_visible(true);
                return;
            }
        }
        install_request_action();
    });

    let cancel_install_window = window.as_weak();
    window.on_cancel_update_install(move || {
        if let Some(window) = cancel_install_window.upgrade() {
            window.set_update_install_confirm_visible(false);
        }
    });

    let discard_install_window = window.as_weak();
    let discard_install_action = install_update.clone();
    window.on_install_update_without_saving(move || {
        if let Some(window) = discard_install_window.upgrade() {
            window.set_update_install_confirm_visible(false);
            discard_install_action();
        }
    });

    let save_install_window = window.as_weak();
    let save_install_session = session.clone();
    let save_install_devices = logical_devices.clone();
    let save_install_saver = saver.clone();
    let save_install_action = install_update;
    window.on_save_and_install_update(move || {
        let Some(window) = save_install_window.upgrade() else {
            return;
        };
        let dirty_settings = save_install_session
            .borrow()
            .dirty_settings(&save_install_devices.borrow());
        let result = match &save_install_saver {
            Some(saver) => dirty_settings
                .iter()
                .try_for_each(|(settings_id, settings)| {
                    saver(settings_id.as_deref(), settings).map(|_| ())
                }),
            None => Err(DogiError::BackendUnavailable(
                "mouse settings storage is unavailable".to_owned(),
            )),
        };
        if let Err(error) = result {
            window.set_update_install_confirm_visible(false);
            set_window_status(
                &window,
                UiStatus::presentation(UiStatusKind::Error, UiMessage::SaveFailed)
                    .with_detail(error.to_string()),
            );
            return;
        }
        save_install_session.borrow_mut().mark_all_saved();
        window.set_draft_dirty(false);
        window.set_update_install_confirm_visible(false);
        save_install_action();
    });

    let active_automatic_update_checks = Rc::new(Cell::new(automatic_update_checks_enabled));
    let automatic_update_save = preferences.save.clone();
    let automatic_update_window = window.as_weak();
    let selected_automatic_update_checks = active_automatic_update_checks.clone();
    window.on_automatic_update_checks_enabled_changed(move |enabled| {
        let Some(window) = automatic_update_window.upgrade() else {
            return;
        };
        if !persist_application_setting(
            &window,
            &automatic_update_save,
            ApplicationPreferenceChange::AutomaticUpdateChecksEnabled(enabled),
        ) {
            window.set_automatic_update_checks_enabled(selected_automatic_update_checks.get());
            return;
        }
        selected_automatic_update_checks.set(enabled);
        window.set_automatic_update_checks_enabled(enabled);
        if enabled && window.get_update_supported() {
            window.invoke_automatic_update_check();
        }
    });

    let update_schedule_timer = Rc::new(slint::Timer::default());
    let scheduled_update_window = window.as_weak();
    update_schedule_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(60 * 60),
        move || {
            let Some(window) = scheduled_update_window.upgrade() else {
                return;
            };
            if window.get_automatic_update_checks_enabled() && window.get_update_supported() {
                window.invoke_automatic_update_check();
            }
        },
    );
    if update_worker_available && automatic_update_checks_enabled {
        let startup_update_window = window.as_weak();
        slint::Timer::single_shot(Duration::from_secs(2), move || {
            if let Some(window) = startup_update_window.upgrade()
                && window.get_automatic_update_checks_enabled()
            {
                window.invoke_automatic_update_check();
            }
        });
    }

    let active_language = Rc::new(Cell::new(language));
    let language_save = preferences.save.clone();
    let language_window = window.as_weak();
    window.on_language_selected(move |index| {
        let language = ApplicationLanguage::from_index(index);
        request_language_change(
            language_window.clone(),
            active_language.clone(),
            language_save.clone(),
            language,
        );
    });

    let theme_save = preferences.save.clone();
    let theme_window = window.as_weak();
    window.on_theme_selected(move |index| {
        let Some(window) = theme_window.upgrade() else {
            return;
        };
        let theme = ApplicationTheme::from_index(index);
        window.set_theme_index(theme.index());
        persist_application_setting(
            &window,
            &theme_save,
            ApplicationPreferenceChange::Theme(theme),
        );
    });

    let close_save = preferences.save;
    let close_window = window.as_weak();
    let close_tray = tray.as_ref().map(|tray| tray.as_weak());
    let selected_close_behavior = active_close_behavior;
    window.on_close_behavior_selected(move |index| {
        let Some(window) = close_window.upgrade() else {
            return;
        };
        let close_behavior = CloseBehavior::from_index(index);
        let tray = close_tray.as_ref().and_then(|tray| tray.upgrade());
        if close_behavior == CloseBehavior::MinimizeToTray && tray.is_none() {
            window.set_close_behavior_index(CloseBehavior::Quit.index());
            set_window_status(
                &window,
                UiStatus::presentation(UiStatusKind::Warning, UiMessage::TrayUnavailable),
            );
            return;
        }
        if let Some(tray) = tray {
            tray.set_enabled(close_behavior == CloseBehavior::MinimizeToTray);
        }
        selected_close_behavior.set(close_behavior);
        window.set_close_behavior_index(close_behavior.index());
        persist_application_setting(
            &window,
            &close_save,
            ApplicationPreferenceChange::CloseBehavior(close_behavior),
        );
    });

    let pending_apply = Rc::new(RefCell::new(None::<PendingApplyConfirmation>));

    let edit_session = session.clone();
    let edit_pending_apply = pending_apply.clone();
    let edit_window = window.as_weak();
    window.on_settings_edited(move || {
        if let Some(window) = edit_window.upgrade() {
            let mut session = edit_session.borrow_mut();
            session.capture_window(&window);
            window.set_draft_dirty(session.current_dirty());
            edit_pending_apply.borrow_mut().take();
            window.set_confirm_visible(false);
            set_window_status(&window, UiStatus::default());
        }
    });

    let (scan_sender, scan_receiver) = mpsc::channel::<DeviceScanCompletion>();
    let scan_in_flight = Rc::new(Cell::new(false));
    let scan_intent = Rc::new(Cell::new(DeviceScanIntent::User));
    let hidpp_recovery_attempt = Rc::new(Cell::new(0_usize));
    let hidpp_recovery_generation = Rc::new(Cell::new(0_u64));
    let rescan_timer = Rc::new(slint::Timer::default());
    let timer_control = Rc::downgrade(&rescan_timer);
    let timer_loader = loader.clone();
    let timer_devices = logical_devices.clone();
    let timer_session = session.clone();
    let timer_pending_apply = pending_apply.clone();
    let timer_in_flight = scan_in_flight.clone();
    let timer_scan_intent = scan_intent.clone();
    let timer_recovery_attempt = hidpp_recovery_attempt.clone();
    let timer_recovery_generation = hidpp_recovery_generation.clone();
    let timer_runtime_startup_pending = runtime_startup_pending.clone();
    let timer_runtime_sender = runtime_work_sender.clone();
    let timer_runtime_timer = runtime_timer.clone();
    let timer_window = window.as_weak();
    if discovery.is_some() {
        rescan_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(40),
            move || {
                let mut completion = match scan_receiver.try_recv() {
                    Ok(completion) => completion,
                    Err(mpsc::TryRecvError::Empty) => return,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if let Some(timer) = timer_control.upgrade() {
                            timer.stop();
                        }
                        timer_in_flight.set(false);
                        if let Some(window) = timer_window.upgrade() {
                            window.set_rescan_in_progress(false);
                            set_window_status(
                                &window,
                                UiStatus::presentation(
                                    UiStatusKind::Error,
                                    UiMessage::ScannerStopped,
                                ),
                            );
                        }
                        return;
                    }
                };
                if completion.phase == DeviceScanPhase::Inventory
                    && let Ok(enriched) = scan_receiver.try_recv()
                {
                    debug_assert_eq!(enriched.intent, completion.intent);
                    debug_assert_eq!(enriched.phase, DeviceScanPhase::Enriched);
                    completion = enriched;
                }

                let Some(window) = timer_window.upgrade() else {
                    return;
                };
                let enriched = completion.phase == DeviceScanPhase::Enriched;
                if enriched {
                    if let Some(timer) = timer_control.upgrade() {
                        timer.stop();
                    }
                    timer_in_flight.set(false);
                    if completion.intent == DeviceScanIntent::User {
                        window.set_rescan_in_progress(false);
                    }
                    if timer_runtime_startup_pending.replace(false) {
                        timer_runtime_timer.restart();
                        if !dispatch_desktop_runtime_operation(
                            &window,
                            timer_runtime_sender.as_ref(),
                            DesktopRuntimeOperation::Reconcile {
                                enabled: background_operations_enabled,
                            },
                        ) {
                            timer_runtime_timer.stop();
                            window.set_runtime_busy(false);
                            window.set_runtime_state(DesktopRuntimeState::Degraded);
                            window.set_runtime_detail("Dogi runtime manager is unavailable".into());
                        }
                    }
                }

                let scanned = match completion.result {
                    Ok(devices) => devices,
                    Err(error) => {
                        if !enriched {
                            return;
                        }
                        if completion.intent == DeviceScanIntent::User {
                            set_window_status(
                                &window,
                                UiStatus::presentation(UiStatusKind::Error, UiMessage::ScanFailed)
                                    .with_detail(error.to_string()),
                            );
                        } else if devices_need_hidpp_recovery(&timer_devices.borrow()) {
                            schedule_hidpp_recovery_scan(
                                timer_window.clone(),
                                timer_session.clone(),
                                timer_in_flight.clone(),
                                timer_scan_intent.clone(),
                                timer_recovery_attempt.clone(),
                                timer_recovery_generation.clone(),
                            );
                        }
                        return;
                    }
                };

                if completion.intent == DeviceScanIntent::HidppRecovery
                    && (timer_session.borrow().any_dirty() || window.get_confirm_visible())
                {
                    return;
                }

                let next_devices = crate::logical_devices(&scanned);
                let mut session = timer_session.borrow_mut();
                session.capture_window(&window);
                if let Err(error) = session.reconcile_devices(&next_devices, timer_loader.as_ref())
                {
                    if completion.intent == DeviceScanIntent::User {
                        set_window_status(
                            &window,
                            UiStatus::presentation(UiStatusKind::Error, UiMessage::ScanFailed)
                                .with_detail(error.to_string()),
                        );
                    }
                    return;
                }

                let rows = next_devices
                    .iter()
                    .map(|device| device_row_from_logical_with_discovery(device, !enriched))
                    .collect::<Vec<_>>();
                *timer_devices.borrow_mut() = next_devices;
                window.set_devices(Rc::new(slint::VecModel::from(rows)).into());
                if completion.intent == DeviceScanIntent::User && enriched {
                    timer_pending_apply.borrow_mut().take();
                    window.set_confirm_visible(false);
                }

                let devices = timer_devices.borrow();
                refresh_selected_device_view_with_discovery(&window, &devices, &session, !enriched);
                if completion.intent == DeviceScanIntent::User && enriched {
                    let status = if session.selected_index.is_some() {
                        UiStatus::default()
                    } else {
                        UiStatus::presentation(UiStatusKind::Neutral, UiMessage::NoDevices)
                    };
                    set_window_status(&window, status);
                }

                if !enriched {
                    return;
                }

                if devices_need_hidpp_recovery(&devices) {
                    schedule_hidpp_recovery_scan(
                        timer_window.clone(),
                        timer_session.clone(),
                        timer_in_flight.clone(),
                        timer_scan_intent.clone(),
                        timer_recovery_attempt.clone(),
                        timer_recovery_generation.clone(),
                    );
                } else {
                    timer_recovery_attempt.set(0);
                    timer_recovery_generation.set(timer_recovery_generation.get().wrapping_add(1));
                }
            },
        );
        rescan_timer.stop();
    }

    let rescan_discovery = discovery.clone();
    let rescan_session = session.clone();
    let rescan_in_flight = scan_in_flight.clone();
    let rescan_intent = scan_intent;
    let rescan_recovery_attempt = hidpp_recovery_attempt;
    let rescan_recovery_generation = hidpp_recovery_generation;
    let rescan_runtime_startup_pending = runtime_startup_pending;
    let rescan_runtime_sender = runtime_work_sender;
    let rescan_runtime_timer = runtime_timer.clone();
    let rescan_sender = scan_sender;
    let rescan_poll_timer = rescan_timer.clone();
    let rescan_window = window.as_weak();
    window.on_rescan_devices(move || {
        let Some(window) = rescan_window.upgrade() else {
            return;
        };
        if window.get_horizontal_scroll_test_open() {
            window.set_horizontal_scroll_test_open(false);
            window.invoke_horizontal_scroll_test_toggled(false);
        }
        let Some(discovery) = &rescan_discovery else {
            return;
        };
        let intent = rescan_intent.replace(DeviceScanIntent::User);
        if rescan_in_flight.replace(true) {
            return;
        }

        if intent == DeviceScanIntent::User {
            rescan_recovery_attempt.set(0);
            rescan_recovery_generation.set(rescan_recovery_generation.get().wrapping_add(1));
        }
        rescan_session.borrow_mut().capture_window(&window);
        if intent == DeviceScanIntent::User {
            window.set_rescan_in_progress(true);
            set_window_status(
                &window,
                UiStatus::presentation(UiStatusKind::Info, UiMessage::Scanning),
            );
        }
        rescan_poll_timer.restart();
        let discovery = discovery.clone();
        let sender = rescan_sender.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("dogi-device-scan".to_owned())
            .spawn(move || {
                run_device_discovery(discovery, intent, &sender);
            })
        {
            rescan_in_flight.set(false);
            rescan_poll_timer.stop();
            if rescan_runtime_startup_pending.replace(false) {
                rescan_runtime_timer.restart();
                if !dispatch_desktop_runtime_operation(
                    &window,
                    rescan_runtime_sender.as_ref(),
                    DesktopRuntimeOperation::Reconcile {
                        enabled: background_operations_enabled,
                    },
                ) {
                    rescan_runtime_timer.stop();
                    window.set_runtime_busy(false);
                    window.set_runtime_state(DesktopRuntimeState::Degraded);
                    window.set_runtime_detail("Dogi runtime manager is unavailable".into());
                }
            }
            if intent == DeviceScanIntent::User {
                window.set_rescan_in_progress(false);
                set_window_status(
                    &window,
                    UiStatus::presentation(UiStatusKind::Error, UiMessage::ScanStartFailed)
                        .with_detail(error.to_string()),
                );
            }
        }
    });

    if discovery.is_some() && logical_devices.borrow().is_empty() {
        let initial_scan_window = window.as_weak();
        slint::Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = initial_scan_window.upgrade() {
                window.invoke_rescan_devices();
            }
        });
    }

    let select_session = session.clone();
    let select_devices = logical_devices.clone();
    let select_window = window.as_weak();
    let select_pending_apply = pending_apply.clone();
    window.on_select_device(move |index| {
        if let Some(window) = select_window.upgrade() {
            if window.get_horizontal_scroll_test_open() {
                window.set_horizontal_scroll_test_open(false);
                window.invoke_horizontal_scroll_test_toggled(false);
            }
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            let mut session = select_session.borrow_mut();
            session.capture_window(&window);
            if !session.select(index) {
                return;
            }
            select_pending_apply.borrow_mut().take();
            window.set_confirm_visible(false);
            let devices = select_devices.borrow();
            refresh_selected_device_view(&window, &devices, &session);
            if let Some(device) = devices.get(index) {
                set_window_status(
                    &window,
                    UiStatus::presentation(UiStatusKind::Info, UiMessage::EditingDevice)
                        .with_subject(logical_device_name(device)),
                );
            }
        }
    });

    let revert_session = session.clone();
    let revert_devices = logical_devices.clone();
    let revert_window = window.as_weak();
    let revert_pending_apply = pending_apply.clone();
    window.on_revert_current_device(move || {
        if let Some(window) = revert_window.upgrade() {
            if window.get_horizontal_scroll_test_open() {
                window.set_horizontal_scroll_test_open(false);
                window.invoke_horizontal_scroll_test_toggled(false);
            }
            let mut session = revert_session.borrow_mut();
            session.revert_current();
            revert_pending_apply.borrow_mut().take();
            window.set_confirm_visible(false);
            let devices = revert_devices.borrow();
            refresh_selected_device_view(&window, &devices, &session);
            set_window_status(
                &window,
                UiStatus::presentation(UiStatusKind::Info, UiMessage::ChangesReverted),
            );
        }
    });

    let save_session = session.clone();
    let save_devices = logical_devices.clone();
    let save_window = window.as_weak();
    let save_saver = saver.clone();
    let save_pending_apply = pending_apply.clone();
    window.on_save_for_later(move || {
        if let Some(window) = save_window.upgrade() {
            let target = capture_settings_target(&window, &save_session, &save_devices);
            refresh_settings_view(
                &window,
                target.device_id().unwrap_or("no-device"),
                &target.settings,
            );
            save_pending_apply.borrow_mut().take();
            window.set_confirm_visible(false);
            let (status, saved) = match &save_saver {
                Some(saver) => match saver(target.settings_id.as_deref(), &target.settings) {
                    Ok(path) => (
                        UiStatus::presentation(
                            UiStatusKind::Success,
                            UiMessage::ChangesSavedForLater,
                        )
                        .with_path(path),
                        true,
                    ),
                    Err(error) => (
                        UiStatus::presentation(UiStatusKind::Error, UiMessage::SaveFailed)
                            .with_detail(error.to_string()),
                        false,
                    ),
                },
                None => (
                    UiStatus::presentation(UiStatusKind::Success, UiMessage::ChangesSavedForLater),
                    true,
                ),
            };
            if saved {
                save_session.borrow_mut().mark_current_saved();
                window.set_draft_dirty(false);
            }
            set_window_status(&window, status);
        }
    });

    let profile_session = session.clone();
    let profile_devices = logical_devices.clone();
    let profile_window = window.as_weak();
    let profile_pending_apply = pending_apply.clone();
    window.on_save_app_profile(move || {
        if let Some(window) = profile_window.upgrade() {
            let mut session = profile_session.borrow_mut();
            let mut settings = session.capture_window(&window);
            match upsert_app_profile_from_window(&window, &mut settings) {
                Ok(_) => {
                    session.replace_current(settings.clone());
                    let plan_device_id = session
                        .selected_index
                        .and_then(|index| {
                            profile_devices
                                .borrow()
                                .get(index)
                                .map(|device| device.primary.id.clone())
                        })
                        .unwrap_or_else(|| "no-device".to_owned());
                    profile_pending_apply.borrow_mut().take();
                    window.set_confirm_visible(false);
                    refresh_settings_view(&window, &plan_device_id, &settings);
                    window.set_draft_dirty(session.current_dirty());
                    set_window_status(&window, UiStatus::default());
                }
                Err(error) => set_window_status(&window, error.status()),
            }
        }
    });

    let remove_profile_session = session.clone();
    let remove_profile_devices = logical_devices.clone();
    let remove_profile_window = window.as_weak();
    let remove_profile_pending_apply = pending_apply.clone();
    window.on_remove_app_profile(move || {
        if let Some(window) = remove_profile_window.upgrade() {
            let mut session = remove_profile_session.borrow_mut();
            let mut settings = session.capture_window(&window);
            match remove_app_profile_from_window(&window, &mut settings) {
                Ok(_) => {
                    session.replace_current(settings.clone());
                    let plan_device_id = session
                        .selected_index
                        .and_then(|index| {
                            remove_profile_devices
                                .borrow()
                                .get(index)
                                .map(|device| device.primary.id.clone())
                        })
                        .unwrap_or_else(|| "no-device".to_owned());
                    remove_profile_pending_apply.borrow_mut().take();
                    window.set_confirm_visible(false);
                    refresh_settings_view(&window, &plan_device_id, &settings);
                    set_app_profile_editor_from_settings(&window, &settings);
                    window.set_draft_dirty(session.current_dirty());
                    set_window_status(&window, UiStatus::default());
                }
                Err(error) => set_window_status(&window, error.status()),
            }
        }
    });

    let select_profile_session = session.clone();
    let select_profile_window = window.as_weak();
    window.on_select_app_profile(move |index| {
        if let Some(window) = select_profile_window.upgrade() {
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            if let Some(profile) = select_profile_session
                .borrow()
                .current()
                .app_profiles
                .get(index)
            {
                set_app_profile_editor_from_profile(&window, profile);
                window.set_selected_app_profile_index(index as i32);
                window.set_confirm_visible(false);
            }
        }
    });

    let apply_session = session.clone();
    let apply_devices = logical_devices.clone();
    let apply_window = window.as_weak();
    let apply_saver = saver;
    let apply_applier = applier;
    let apply_pending_apply = pending_apply.clone();
    window.on_apply_requested(move || {
        if let Some(window) = apply_window.upgrade() {
            if window.get_horizontal_scroll_test_open() {
                window.set_horizontal_scroll_test_open(false);
                window.invoke_horizontal_scroll_test_toggled(false);
            }
            let target = capture_settings_target(&window, &apply_session, &apply_devices);
            let saved_settings = apply_session.borrow().current_saved().clone();
            let has_unsaved_changes = target.settings != saved_settings;
            if target
                .device
                .as_ref()
                .is_none_or(|device| !logical_device_is_ready(device))
            {
                apply_pending_apply.borrow_mut().take();
                window.set_confirm_visible(false);
                set_window_status(
                    &window,
                    UiStatus::presentation(UiStatusKind::Error, UiMessage::ApplyUnavailable),
                );
                return;
            }
            let device_id = target.device_id();
            let plan = device_apply_plan(
                device_id.unwrap_or("no-device"),
                &saved_settings,
                &target.settings,
            );
            let plan_rows = device_plan_rows_from_plan(&plan, &target.settings);
            let step_count = plan_rows.len();
            window.set_plan_rows(Rc::new(slint::VecModel::from(plan_rows)).into());

            let Some(device_id) = device_id else {
                apply_pending_apply.borrow_mut().take();
                window.set_confirm_visible(false);
                set_window_status(
                    &window,
                    UiStatus::presentation(UiStatusKind::Error, UiMessage::NoDevice)
                        .with_count(step_count),
                );
                return;
            };

            if !plan.steps.is_empty() {
                if apply_applier.is_none() {
                    apply_pending_apply.borrow_mut().take();
                    window.set_confirm_visible(false);
                    set_window_status(
                        &window,
                        UiStatus::presentation(
                            UiStatusKind::Error,
                            UiMessage::ApplyBackendUnavailable,
                        )
                        .with_count(step_count),
                    );
                    return;
                }

                let confirmation = PendingApplyConfirmation::new(
                    Some(device_id.to_owned()),
                    target.settings.clone(),
                );
                if !pending_apply_matches(&apply_pending_apply.borrow(), &confirmation) {
                    *apply_pending_apply.borrow_mut() = Some(confirmation);
                    window.set_confirm_change_count(i32::try_from(step_count).unwrap_or(i32::MAX));
                    window.set_confirm_visible(true);
                    set_window_status(
                        &window,
                        UiStatus::presentation(UiStatusKind::Info, UiMessage::ReviewPlan)
                            .with_count(step_count),
                    );
                    return;
                }
            }

            apply_pending_apply.borrow_mut().take();
            window.set_confirm_visible(false);
            let saved_path = if !has_unsaved_changes {
                None
            } else {
                match &apply_saver {
                    Some(saver) => match saver(target.settings_id.as_deref(), &target.settings) {
                        Ok(path) => Some(path),
                        Err(error) => {
                            window.set_confirm_visible(false);
                            set_window_status(
                                &window,
                                UiStatus::presentation(UiStatusKind::Error, UiMessage::SaveFailed)
                                    .with_detail(error.to_string()),
                            );
                            return;
                        }
                    },
                    None => None,
                }
            };

            apply_session.borrow_mut().mark_current_saved();
            window.set_draft_dirty(false);

            if plan.steps.is_empty() {
                set_window_status(
                    &window,
                    UiStatus::presentation(UiStatusKind::Success, UiMessage::ChangesSaved),
                );
                return;
            }

            let applier = apply_applier
                .as_ref()
                .expect("device apply backend checked before confirmation");
            match applier(device_id, &target.settings, &plan) {
                Ok(report) => {
                    let failed = count_apply_status(&report, SettingsApplyStatus::Failed);
                    let applied = count_apply_status(&report, SettingsApplyStatus::Applied);
                    let unsupported = count_apply_status(&report, SettingsApplyStatus::Unsupported);
                    let status_kind = if failed > 0 {
                        UiStatusKind::Error
                    } else if unsupported > 0 {
                        UiStatusKind::Warning
                    } else {
                        UiStatusKind::Success
                    };
                    let mut status = UiStatus::presentation(status_kind, UiMessage::ApplySummary)
                        .with_apply_counts(failed, applied, unsupported);
                    if let Some(path) = saved_path {
                        status = status.with_path(path);
                    }
                    set_window_status(&window, status);
                }
                Err(error) => {
                    let mut status =
                        UiStatus::presentation(UiStatusKind::Error, UiMessage::ApplyFailed)
                            .with_detail(error.to_string());
                    if let Some(path) = saved_path {
                        status = status.with_path(path);
                    }
                    set_window_status(&window, status);
                }
            }
        }
    });

    let cancel_pending_apply = pending_apply;
    let cancel_window = window.as_weak();
    window.on_cancel_apply(move || {
        if let Some(window) = cancel_window.upgrade() {
            cancel_pending_apply.borrow_mut().take();
            window.set_confirm_visible(false);
            set_window_status(&window, UiStatus::default());
        }
    });

    let result = window
        .run()
        .map_err(|error| DogiError::Ui(error.to_string()));
    let _ = dispatch_horizontal_scroll_preview(
        preview_work_sender.as_ref(),
        &preview_sequence,
        HorizontalScrollPreviewCommand::Clear,
    );
    runtime_timer.stop();
    update_poll_timer.stop();
    update_schedule_timer.stop();
    preview_poll_timer.stop();
    preview_speed_timer.stop();
    preview_heartbeat_timer.stop();
    rescan_timer.stop();
    result
}

fn refresh_selected_device_view(
    window: &MainWindow,
    devices: &[LogicalDevice],
    session: &DeviceUiSession,
) {
    refresh_selected_device_view_with_discovery(window, devices, session, false);
}

fn refresh_selected_device_view_with_discovery(
    window: &MainWindow,
    devices: &[LogicalDevice],
    session: &DeviceUiSession,
    enriching: bool,
) {
    let settings = session.current();
    let selected_device = session.selected_index.and_then(|index| devices.get(index));

    window.set_selected_device_index(
        session
            .selected_index
            .and_then(|index| i32::try_from(index).ok())
            .unwrap_or(-1),
    );
    window.set_selected_device(
        selected_device
            .map(|device| device_row_from_logical_with_discovery(device, enriching))
            .unwrap_or_default(),
    );
    set_settings_controls(window, settings);
    window.set_draft_dirty(session.current_dirty());
    refresh_settings_view(
        window,
        selected_device
            .map(|device| device.primary.id.as_str())
            .unwrap_or("no-device"),
        settings,
    );
}

fn set_settings_controls(window: &MainWindow, settings: &Master3sSettings) {
    window.set_pointer_speed(f32::from(settings.pointer_speed_percent));
    window.set_smart_shift_threshold(f32::from(settings.smart_shift_threshold));
    window.set_high_resolution_scroll(settings.high_resolution_scroll);
    window.set_scroll_direction_index(i32::from(settings.natural_scroll));
    window.set_wheel_mode_index(wheel_mode_index(settings.ratchet_mode));
    window.set_thumb_wheel_index(thumb_wheel_index(settings.thumb_wheel));
    window.set_thumb_wheel_speed(f32::from(settings.thumb_wheel_speed_percent));
    window.set_back_action_index(action_index(settings.button_action(Master3sButton::Back)));
    window.set_forward_action_index(action_index(
        settings.button_action(Master3sButton::Forward),
    ));
    window.set_gesture_action_index(action_index(
        settings.button_action(Master3sButton::Gesture),
    ));
    window.set_mode_shift_action_index(action_index(
        settings.button_action(Master3sButton::ModeShift),
    ));
    window.set_middle_action_index(action_index(settings.button_action(Master3sButton::Middle)));
    set_app_profile_editor_from_settings(window, settings);
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingApplyConfirmation {
    device_id: Option<String>,
    settings: Master3sSettings,
}

impl PendingApplyConfirmation {
    fn new(device_id: Option<String>, settings: Master3sSettings) -> Self {
        Self {
            device_id,
            settings: settings.normalized(),
        }
    }
}

fn pending_apply_matches(
    pending: &Option<PendingApplyConfirmation>,
    current: &PendingApplyConfirmation,
) -> bool {
    pending.as_ref().is_some_and(|pending| pending == current)
}

fn schedule_hidpp_recovery_scan(
    window: slint::Weak<MainWindow>,
    session: Rc<RefCell<DeviceUiSession>>,
    scan_in_flight: Rc<Cell<bool>>,
    scan_intent: Rc<Cell<DeviceScanIntent>>,
    attempt: Rc<Cell<usize>>,
    generation: Rc<Cell<u64>>,
) {
    let attempt_index = attempt.get();
    let Some(delay) = HIDPP_RECOVERY_SCAN_DELAYS.get(attempt_index).copied() else {
        return;
    };
    attempt.set(attempt_index + 1);

    let scheduled_generation = generation.get().wrapping_add(1);
    generation.set(scheduled_generation);
    slint::Timer::single_shot(delay, move || {
        if generation.get() != scheduled_generation
            || scan_in_flight.get()
            || session.borrow().any_dirty()
        {
            return;
        }
        let Some(window) = window.upgrade() else {
            return;
        };
        if window.get_confirm_visible() {
            return;
        }

        scan_intent.set(DeviceScanIntent::HidppRecovery);
        window.invoke_rescan_devices();
    });
}

fn count_apply_status(report: &SettingsApplyReport, status: SettingsApplyStatus) -> usize {
    report
        .outcomes
        .iter()
        .filter(|outcome| outcome.status == status)
        .count()
}

#[derive(Clone, Debug)]
struct LogicalDevice {
    primary: DeviceInfo,
    interface_count: usize,
    hidpp_count: usize,
    endpoints: Vec<String>,
}

struct SettingsTarget {
    settings: Master3sSettings,
    device: Option<LogicalDevice>,
    settings_id: Option<String>,
}

impl SettingsTarget {
    fn device_id(&self) -> Option<&str> {
        self.device
            .as_ref()
            .map(|device| device.primary.id.as_str())
    }
}

fn capture_settings_target(
    window: &MainWindow,
    session: &Rc<RefCell<DeviceUiSession>>,
    devices: &Rc<RefCell<Vec<LogicalDevice>>>,
) -> SettingsTarget {
    let (settings, selected_index, known_settings_id) = {
        let mut session = session.borrow_mut();
        let settings = session.capture_window(window);
        let selected_index = session.selected_index;
        let known_settings_id = selected_index
            .and_then(|index| session.drafts.get(index))
            .and_then(|draft| draft.strong_key.clone());
        (settings, selected_index, known_settings_id)
    };
    let device = selected_index.and_then(|index| devices.borrow().get(index).cloned());
    let settings_id = known_settings_id.or_else(|| {
        device
            .as_ref()
            .map(|device| device_settings_id(&device.primary))
    });

    SettingsTarget {
        settings,
        device,
        settings_id,
    }
}

fn logical_devices(devices: &[DeviceInfo]) -> Vec<LogicalDevice> {
    let mut groups = Vec::<(String, LogicalDevice)>::new();

    for device in devices {
        let key = logical_device_key(device);

        if let Some((_, group)) = groups.iter_mut().find(|(group_key, _)| group_key == &key) {
            group.interface_count += 1;
            if device.capabilities.hidpp == CapabilityState::Supported {
                group.hidpp_count += 1;
            }
            group.endpoints.push(device.path.clone());

            if should_prefer_device(device, &group.primary) {
                group.primary = device.clone();
            }
        } else {
            groups.push((
                key,
                LogicalDevice {
                    primary: device.clone(),
                    interface_count: 1,
                    hidpp_count: usize::from(
                        device.capabilities.hidpp == CapabilityState::Supported,
                    ),
                    endpoints: vec![device.path.clone()],
                },
            ));
        }
    }

    groups.into_iter().map(|(_, group)| group).collect()
}

fn devices_need_hidpp_recovery(devices: &[LogicalDevice]) -> bool {
    devices.iter().any(|device| {
        device.hidpp_count > 0
            && device.primary.access.hidraw_readwrite
            && logical_device_is_master3s(device)
            && device.primary.battery.level_percent.is_none()
    })
}

fn logical_device_key(device: &DeviceInfo) -> String {
    if let Some(physical_path) = &device.physical_path {
        if let Some((base, _)) = physical_path.split_once("/input") {
            return format!("{}:{}:{base}", device.vendor_id, device.product_id);
        }

        return format!("{}:{}:{physical_path}", device.vendor_id, device.product_id);
    }

    if let Some(serial_number) = &device.serial_number {
        return format!("{}:{}:{serial_number}", device.vendor_id, device.product_id);
    }

    format!(
        "{}:{}:{}",
        device.vendor_id, device.product_id, device.sysfs_path
    )
}

fn logical_device_transport_key(device: &LogicalDevice) -> String {
    logical_device_key(&device.primary)
}

fn logical_device_strong_key(device: &LogicalDevice) -> Option<String> {
    let settings_id = device_settings_id(&device.primary);
    (settings_id != device.primary.id).then_some(settings_id)
}

fn should_prefer_device(candidate: &DeviceInfo, current: &DeviceInfo) -> bool {
    let candidate_paired = candidate.paired_device.is_some();
    let current_paired = current.paired_device.is_some();
    let candidate_hidpp = candidate.capabilities.hidpp == CapabilityState::Supported;
    let current_hidpp = current.capabilities.hidpp == CapabilityState::Supported;

    if candidate_paired != current_paired {
        return candidate_paired;
    }

    candidate_hidpp && !current_hidpp
}

fn device_row_from_logical(device: &LogicalDevice) -> DeviceRow {
    device_row_from_logical_with_discovery(device, false)
}

fn device_row_from_logical_with_discovery(device: &LogicalDevice, enriching: bool) -> DeviceRow {
    let discovery_state = if enriching
        && device.hidpp_count > 0
        && device.primary.access.hidraw_readwrite
        && device.primary.paired_device.is_none()
    {
        DeviceDiscoveryState::Enriching
    } else {
        DeviceDiscoveryState::Idle
    };

    DeviceRow {
        id: device.primary.id.clone().into(),
        name: logical_device_name(device).into(),
        path: device.primary.path.clone().into(),
        protocol: device
            .primary
            .paired_device
            .as_ref()
            .and_then(|paired| paired.protocol)
            .map(|protocol| protocol.to_string())
            .unwrap_or_default()
            .into(),
        connection: device_connection(device.primary.connection),
        access_state: logical_device_access_state(device),
        discovery_state,
        battery_level: device
            .primary
            .battery
            .level_percent
            .map(i32::from)
            .unwrap_or(-1),
        battery_state: battery_state(device.primary.battery.status),
        interface_count: i32::try_from(device.interface_count).unwrap_or(i32::MAX),
        is_mouse: logical_device_is_mouse(device),
    }
}

fn device_connection(connection: ConnectionKind) -> DeviceConnection {
    match connection {
        ConnectionKind::Usb => DeviceConnection::Usb,
        ConnectionKind::Bluetooth => DeviceConnection::Bluetooth,
        ConnectionKind::Unifying => DeviceConnection::Unifying,
        ConnectionKind::Bolt => DeviceConnection::Bolt,
        ConnectionKind::Lightspeed => DeviceConnection::Lightspeed,
        ConnectionKind::Unknown => DeviceConnection::Unknown,
    }
}

fn battery_state(status: BatteryStatus) -> BatteryState {
    match status {
        BatteryStatus::Charging => BatteryState::Charging,
        BatteryStatus::Discharging => BatteryState::Discharging,
        BatteryStatus::Full => BatteryState::Full,
        BatteryStatus::Low => BatteryState::Low,
        BatteryStatus::Critical => BatteryState::Critical,
        BatteryStatus::Offline => BatteryState::Offline,
        BatteryStatus::Unknown => BatteryState::Unknown,
    }
}

fn logical_device_name(device: &LogicalDevice) -> String {
    if let Some(name) = resolved_logitech_device_name(&device.primary) {
        return name.to_owned();
    }

    if is_receiver_endpoint(&device.primary) {
        match device.primary.receiver_kind {
            Some(dogi_core::ReceiverKind::Bolt) => "Logi Bolt receiver".to_owned(),
            Some(dogi_core::ReceiverKind::Unifying) => "Logitech Unifying receiver".to_owned(),
            Some(dogi_core::ReceiverKind::Lightspeed) => "Logitech LIGHTSPEED receiver".to_owned(),
            _ => "Logitech receiver".to_owned(),
        }
    } else {
        device.primary.name.clone()
    }
}

fn is_receiver_endpoint(device: &DeviceInfo) -> bool {
    device.receiver_kind.is_some() || device.name.to_ascii_lowercase().contains("receiver")
}

fn logical_device_access_state(device: &LogicalDevice) -> DeviceAccessState {
    if !device.primary.access.hidraw_readable {
        return if hidraw_node_missing(device) {
            DeviceAccessState::HidDeviceMissing
        } else {
            DeviceAccessState::HidPermissionDenied
        };
    }

    if !device.primary.access.hidraw_readwrite {
        return DeviceAccessState::ReadOnly;
    }

    if device.primary.paired_device.is_none() {
        return DeviceAccessState::NoPairedResponse;
    }

    if !logical_device_is_master3s(device) {
        return DeviceAccessState::Unsupported;
    }

    if logical_device_is_ready(device) {
        DeviceAccessState::Ready
    } else {
        DeviceAccessState::HidppUnavailable
    }
}

fn logical_device_is_ready(device: &LogicalDevice) -> bool {
    device.hidpp_count > 0
        && device.primary.access.hidraw_readwrite
        && device.primary.paired_device.is_some()
        && logical_device_is_master3s(device)
}

fn logical_device_is_mouse(device: &LogicalDevice) -> bool {
    device
        .primary
        .paired_device
        .as_ref()
        .and_then(|paired| paired.kind.as_deref())
        .is_some_and(|kind| kind.eq_ignore_ascii_case("mouse"))
        || !is_receiver_endpoint(&device.primary)
}

fn logical_device_is_master3s(device: &LogicalDevice) -> bool {
    if known_logitech_product_name(device.primary.vendor_id, device.primary.product_id).is_some() {
        return true;
    }

    device.primary.paired_device.as_ref().is_some_and(|paired| {
        known_logitech_wpid_name(paired.wpid.as_deref()).is_some()
            || known_logitech_model_name(paired.model_id.as_deref()).is_some()
    })
}

fn hidraw_node_missing(device: &LogicalDevice) -> bool {
    device
        .primary
        .battery
        .detail
        .as_deref()
        .is_some_and(|detail| detail.contains("No such file or directory"))
}

fn app_profile_row_from_profile(profile: &AppProfile) -> AppProfileRow {
    AppProfileRow {
        app: profile.app_name.clone().into(),
        initial: profile
            .app_name
            .chars()
            .next()
            .map(|character| character.to_uppercase().collect::<String>())
            .unwrap_or_else(|| "?".to_owned())
            .into(),
        pointer_speed: i32::from(profile.pointer_speed_percent),
        thumb_wheel_index: thumb_wheel_index(profile.thumb_wheel),
    }
}

fn refresh_settings_view(window: &MainWindow, plan_device_id: &str, settings: &Master3sSettings) {
    let app_profile_rows = settings
        .app_profiles
        .iter()
        .map(app_profile_row_from_profile)
        .collect::<Vec<_>>();
    let plan = build_master3s_apply_plan(plan_device_id, settings);

    window.set_app_profile_rows(Rc::new(slint::VecModel::from(app_profile_rows)).into());
    window.set_plan_rows(
        Rc::new(slint::VecModel::from(device_plan_rows_from_plan(
            &plan, settings,
        )))
        .into(),
    );
}

fn set_app_profile_editor_from_settings(window: &MainWindow, settings: &Master3sSettings) {
    if let Some(profile) = settings.app_profiles.first() {
        set_app_profile_editor_from_profile(window, profile);
        window.set_selected_app_profile_index(0);
    } else {
        window.set_app_profile_editor_app(String::new().into());
        window.set_app_profile_editor_pointer_speed(100.0);
        window.set_app_profile_editor_thumb_wheel_index(thumb_wheel_index(
            ThumbWheelMode::HorizontalScroll,
        ));
        window.set_selected_app_profile_index(-1);
    }
}

fn set_app_profile_editor_from_profile(window: &MainWindow, profile: &AppProfile) {
    window.set_app_profile_editor_app(profile.app_name.clone().into());
    window.set_app_profile_editor_pointer_speed(f32::from(profile.pointer_speed_percent));
    window.set_app_profile_editor_thumb_wheel_index(thumb_wheel_index(profile.thumb_wheel));
}

fn plan_row_from_step(step: &SettingsApplyStep, settings: &Master3sSettings) -> PlanRow {
    let mut row = PlanRow {
        scope: plan_step_scope(step, settings),
        ..PlanRow::default()
    };

    match &step.operation {
        SettingsApplyOperation::PointerSpeed { percent } => {
            row.kind = PlanKind::PointerSpeed;
            row.value = i32::from(*percent);
        }
        SettingsApplyOperation::WheelBehavior { mode, threshold } => {
            row.kind = PlanKind::WheelBehavior;
            row.value = i32::from(*threshold);
            row.primary_index = wheel_mode_index(*mode);
        }
        SettingsApplyOperation::ScrollBehavior {
            high_resolution,
            natural,
        } => {
            row.kind = PlanKind::ScrollBehavior;
            row.enabled = high_resolution.unwrap_or(settings.high_resolution_scroll);
            row.secondary_enabled = natural.unwrap_or(settings.natural_scroll);
            row.primary_changed = high_resolution.is_some();
            row.secondary_changed = natural.is_some();
        }
        SettingsApplyOperation::ThumbWheel {
            mode,
            speed_percent,
        } => {
            row.kind = PlanKind::ThumbWheel;
            row.primary_index = thumb_wheel_index(*mode);
            row.value = i32::from(*speed_percent);
        }
        SettingsApplyOperation::ButtonMapping { button, action } => {
            row.kind = PlanKind::ButtonRouting;
            row.primary_index = button_index(*button);
            row.secondary_index = action_index(*action);
            row.enabled = dogi_core::button_action_requires_runtime(*button, *action);
        }
        SettingsApplyOperation::LocalRuntime {
            button_count,
            app_profile_count,
            ..
        } => {
            row.kind = PlanKind::LocalRuntime;
            row.value = i32::try_from(*button_count).unwrap_or(i32::MAX);
            row.secondary_value = i32::try_from(*app_profile_count).unwrap_or(i32::MAX);
        }
        SettingsApplyOperation::AppProfile { profile } => {
            row.kind = PlanKind::AppProfile;
            row.name = profile.app_name.clone().into();
            row.value = i32::from(profile.pointer_speed_percent);
            row.primary_index = thumb_wheel_index(profile.thumb_wheel);
        }
    }

    row
}

fn plan_step_scope(step: &SettingsApplyStep, settings: &Master3sSettings) -> PlanScope {
    match settings_apply_step_scope(step, settings) {
        SettingsApplyScope::Device => PlanScope::Device,
        SettingsApplyScope::Local => PlanScope::Local,
        SettingsApplyScope::Unsupported => PlanScope::Unsupported,
    }
}

fn device_plan_rows_from_plan(
    plan: &dogi_core::SettingsApplyPlan,
    settings: &Master3sSettings,
) -> Vec<PlanRow> {
    plan.steps
        .iter()
        .filter(|step| settings_apply_step_scope(step, settings) == SettingsApplyScope::Device)
        .map(|step| plan_row_from_step(step, settings))
        .collect()
}

fn device_apply_plan(
    device_id: &str,
    saved: &Master3sSettings,
    target: &Master3sSettings,
) -> SettingsApplyPlan {
    build_master3s_device_diff_plan(device_id, saved, target)
}

fn settings_from_window(window: &MainWindow, base: &Master3sSettings) -> Master3sSettings {
    let mut settings = base.clone();
    settings.pointer_speed_percent = clamp_percent(window.get_pointer_speed());
    settings.smart_shift_threshold = clamp_u8(window.get_smart_shift_threshold(), 1, 50);
    settings.high_resolution_scroll = window.get_high_resolution_scroll();
    settings.natural_scroll = window.get_scroll_direction_index() == 1;
    settings.ratchet_mode = ratchet_mode_from_index(window.get_wheel_mode_index());
    settings.smart_shift_enabled = settings.ratchet_mode == WheelRatchetMode::SmartShift;
    settings.thumb_wheel = thumb_wheel_mode_from_index(window.get_thumb_wheel_index());
    settings.thumb_wheel_speed_percent = clamp_u16(
        window.get_thumb_wheel_speed(),
        MIN_THUMB_WHEEL_SPEED_PERCENT,
        MAX_THUMB_WHEEL_SPEED_PERCENT,
    );
    settings.set_button_action(
        Master3sButton::Back,
        action_from_index(window.get_back_action_index()),
    );
    settings.set_button_action(
        Master3sButton::Forward,
        action_from_index(window.get_forward_action_index()),
    );
    settings.set_button_action(
        Master3sButton::Gesture,
        action_from_index(window.get_gesture_action_index()),
    );
    settings.set_button_action(
        Master3sButton::ModeShift,
        action_from_index(window.get_mode_shift_action_index()),
    );
    settings.set_button_action(
        Master3sButton::Middle,
        action_from_index(window.get_middle_action_index()),
    );
    settings
}

fn upsert_app_profile_from_window(
    window: &MainWindow,
    settings: &mut Master3sSettings,
) -> std::result::Result<String, ProfileEditError> {
    let app_name = clean_app_profile_name(&window.get_app_profile_editor_app())?;
    let pointer_speed = clamp_percent(window.get_app_profile_editor_pointer_speed());
    let thumb_wheel =
        thumb_wheel_mode_from_index(window.get_app_profile_editor_thumb_wheel_index());
    let profile_name = upsert_app_profile(settings, &app_name, pointer_speed, thumb_wheel)?;
    let profile = settings
        .app_profiles
        .iter()
        .find(|profile| {
            normalize_app_profile_key(&profile.app_name) == normalize_app_profile_key(&profile_name)
        })
        .expect("updated app profile exists");
    set_app_profile_editor_from_profile(window, profile);
    if let Some(index) = settings.app_profiles.iter().position(|profile| {
        normalize_app_profile_key(&profile.app_name) == normalize_app_profile_key(&profile_name)
    }) {
        window.set_selected_app_profile_index(index as i32);
    }

    Ok(profile_name)
}

fn upsert_app_profile(
    settings: &mut Master3sSettings,
    app_name: &str,
    pointer_speed: u8,
    thumb_wheel: ThumbWheelMode,
) -> std::result::Result<String, ProfileEditError> {
    let app_name = clean_app_profile_name(app_name)?;
    let key = normalize_app_profile_key(&app_name);

    if let Some(profile) = settings
        .app_profiles
        .iter_mut()
        .find(|profile| normalize_app_profile_key(&profile.app_name) == key)
    {
        profile.app_name = app_name.clone();
        profile.pointer_speed_percent = pointer_speed;
        profile.thumb_wheel = thumb_wheel;
    } else {
        settings.app_profiles.push(AppProfile {
            app_name: app_name.clone(),
            pointer_speed_percent: pointer_speed,
            thumb_wheel,
        });
    }

    Ok(app_name)
}

fn remove_app_profile_from_window(
    window: &MainWindow,
    settings: &mut Master3sSettings,
) -> std::result::Result<String, ProfileEditError> {
    let app_name = clean_app_profile_name(&window.get_app_profile_editor_app())?;
    remove_app_profile(settings, &app_name)
}

fn remove_app_profile(
    settings: &mut Master3sSettings,
    app_name: &str,
) -> std::result::Result<String, ProfileEditError> {
    let app_name = clean_app_profile_name(app_name)?;
    let key = normalize_app_profile_key(&app_name);
    let original_len = settings.app_profiles.len();
    settings
        .app_profiles
        .retain(|profile| normalize_app_profile_key(&profile.app_name) != key);

    if settings.app_profiles.len() == original_len {
        return Err(ProfileEditError::NotFound(app_name));
    }

    Ok(app_name)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProfileEditError {
    NameRequired,
    NotFound(String),
}

impl ProfileEditError {
    fn status(self) -> UiStatus {
        match self {
            Self::NameRequired => {
                UiStatus::presentation(UiStatusKind::Error, UiMessage::AppNameRequired)
            }
            Self::NotFound(name) => {
                UiStatus::presentation(UiStatusKind::Error, UiMessage::ProfileNotFound)
                    .with_subject(name)
            }
        }
    }
}

fn clean_app_profile_name(value: &str) -> std::result::Result<String, ProfileEditError> {
    let app_name = value.trim();
    if app_name.is_empty() {
        return Err(ProfileEditError::NameRequired);
    }
    Ok(app_name.to_owned())
}

fn normalize_app_profile_key(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn clamp_percent(value: f32) -> u8 {
    clamp_u8(value, 50, 200)
}

fn clamp_u8(value: f32, min: u8, max: u8) -> u8 {
    value.round().clamp(f32::from(min), f32::from(max)) as u8
}

fn clamp_u16(value: f32, min: u16, max: u16) -> u16 {
    value.round().clamp(f32::from(min), f32::from(max)) as u16
}

fn ratchet_mode_from_index(index: i32) -> WheelRatchetMode {
    match index {
        0 => WheelRatchetMode::Ratchet,
        1 => WheelRatchetMode::FreeSpin,
        _ => WheelRatchetMode::SmartShift,
    }
}

fn thumb_wheel_mode_from_index(index: i32) -> ThumbWheelMode {
    match index {
        1 => ThumbWheelMode::TabSwitch,
        2 => ThumbWheelMode::Zoom,
        3 => ThumbWheelMode::Volume,
        4 => ThumbWheelMode::Disabled,
        _ => ThumbWheelMode::HorizontalScroll,
    }
}

fn wheel_mode_index(mode: WheelRatchetMode) -> i32 {
    match mode {
        WheelRatchetMode::Ratchet => 0,
        WheelRatchetMode::FreeSpin => 1,
        WheelRatchetMode::SmartShift => 2,
    }
}

fn thumb_wheel_index(mode: ThumbWheelMode) -> i32 {
    match mode {
        ThumbWheelMode::HorizontalScroll => 0,
        ThumbWheelMode::TabSwitch => 1,
        ThumbWheelMode::Zoom => 2,
        ThumbWheelMode::Volume => 3,
        ThumbWheelMode::Disabled => 4,
    }
}

fn action_from_index(index: i32) -> ButtonAction {
    match index {
        0 => ButtonAction::Back,
        1 => ButtonAction::Forward,
        2 => ButtonAction::Overview,
        3 => ButtonAction::WindowSwitcher,
        4 => ButtonAction::PreviousWorkspace,
        5 => ButtonAction::NextWorkspace,
        6 => ButtonAction::MiddleClick,
        7 => ButtonAction::Copy,
        8 => ButtonAction::Paste,
        9 => ButtonAction::Disabled,
        _ => ButtonAction::Native,
    }
}

fn action_index(action: ButtonAction) -> i32 {
    match action {
        ButtonAction::Back => 0,
        ButtonAction::Forward => 1,
        ButtonAction::Overview => 2,
        ButtonAction::WindowSwitcher => 3,
        ButtonAction::PreviousWorkspace => 4,
        ButtonAction::NextWorkspace => 5,
        ButtonAction::MiddleClick => 6,
        ButtonAction::Copy => 7,
        ButtonAction::Paste => 8,
        ButtonAction::Disabled => 9,
        ButtonAction::Native => 10,
    }
}

fn button_index(button: Master3sButton) -> i32 {
    match button {
        Master3sButton::Back => 0,
        Master3sButton::Forward => 1,
        Master3sButton::Gesture => 2,
        Master3sButton::ModeShift => 3,
        Master3sButton::Middle => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dogi_core::{
        BusKind, ConnectionKind, DeviceAccess, DeviceCapabilities, HidppFeature,
        HidppProtocolVersion, PairedDeviceInfo, ReceiverKind, ReportDescriptorInfo,
        SettingsApplyOutcome, WritePolicy,
    };
    use slint::ComponentHandle;
    use slint_snapshot::{SnapshotRuntime, runtime::ClockMode};
    use std::{cell::Cell, path::Path};

    #[test]
    fn logical_devices_collapse_receiver_interfaces() {
        let devices = vec![
            make_device(
                "/dev/hidraw0",
                "usb-0000:06:00.0-3/input0",
                CapabilityState::Unknown,
            ),
            make_device(
                "/dev/hidraw1",
                "usb-0000:06:00.0-3/input1",
                CapabilityState::Unknown,
            ),
            make_device(
                "/dev/hidraw2",
                "usb-0000:06:00.0-3/input2",
                CapabilityState::Supported,
            ),
            make_device(
                "/dev/hidraw7",
                "usb-0000:06:00.0-3/input3",
                CapabilityState::Unknown,
            ),
        ];

        let logical = logical_devices(&devices);

        assert_eq!(logical.len(), 1);
        assert_eq!(logical[0].interface_count, 4);
        assert_eq!(logical[0].hidpp_count, 1);
        assert_eq!(logical[0].primary.path, "/dev/hidraw2");
    }

    #[test]
    fn logical_device_prefers_paired_mouse_name() {
        let plain_hidpp = make_device(
            "/dev/hidraw2",
            "usb-0000:06:00.0-3/input2",
            CapabilityState::Supported,
        );
        let mut paired_hidpp = plain_hidpp.clone();
        paired_hidpp.path = "/dev/hidraw3".to_owned();
        paired_hidpp.paired_device = Some(PairedDeviceInfo {
            slot: 1,
            name: Some("MX Master 3S".to_owned()),
            kind: Some("mouse".to_owned()),
            wpid: None,
            protocol: Some(HidppProtocolVersion { major: 4, minor: 5 }),
            unit_id: Some("AABBCCDD".to_owned()),
            model_id: Some("B03400000000".to_owned()),
            feature_count: 0,
            features: Vec::new(),
        });

        let logical = logical_devices(&[plain_hidpp, paired_hidpp]);

        assert_eq!(logical.len(), 1);
        assert_eq!(logical_device_name(&logical[0]), "MX Master 3S");
        assert_eq!(logical[0].primary.path, "/dev/hidraw3");
    }

    #[test]
    fn schedules_recovery_only_for_an_accessible_master3s_without_battery() {
        let mut missing_battery =
            make_paired_device("/dev/hidraw2", "usb-0000:06:00.0-3/input2", "AABBCCDD");
        let logical = logical_devices(&[missing_battery.clone()]);
        assert!(devices_need_hidpp_recovery(&logical));

        missing_battery.battery = dogi_core::BatteryInfo {
            level_percent: Some(80),
            status: BatteryStatus::Discharging,
            source: dogi_core::BatterySource::Hidpp,
            detail: None,
        };
        let logical = logical_devices(&[missing_battery.clone()]);
        assert!(!devices_need_hidpp_recovery(&logical));

        missing_battery.battery =
            dogi_core::BatteryInfo::not_queried("battery requires HID++ query");
        missing_battery.access.hidraw_readwrite = false;
        let logical = logical_devices(&[missing_battery]);
        assert!(!devices_need_hidpp_recovery(&logical));

        let receiver = make_device(
            "/dev/hidraw3",
            "usb-0000:06:00.0-4/input2",
            CapabilityState::Supported,
        );
        let logical = logical_devices(&[receiver]);
        assert!(!devices_need_hidpp_recovery(&logical));
    }

    #[test]
    fn logical_device_name_does_not_display_receiver_as_mouse() {
        let endpoint = make_device(
            "/dev/hidraw2",
            "usb-0000:06:00.0-3/input2",
            CapabilityState::Supported,
        );

        let logical = logical_devices(&[endpoint]);
        let row = device_row_from_logical(&logical[0]);

        assert_eq!(logical_device_name(&logical[0]), "Logi Bolt receiver");
        assert_eq!(row.name, "Logi Bolt receiver");
        assert_eq!(row.connection, DeviceConnection::Bolt);
        assert_eq!(row.access_state, DeviceAccessState::HidPermissionDenied);
        assert_eq!(row.path, "/dev/hidraw2");
        assert_eq!(row.interface_count, 1);
        assert_eq!(row.battery_level, -1);
    }

    #[test]
    fn logical_device_reports_missing_hidraw_node_separately_from_permissions() {
        let mut endpoint = make_device(
            "/dev/hidraw2",
            "usb-0000:06:00.0-3/input2",
            CapabilityState::Supported,
        );
        endpoint.battery = dogi_core::BatteryInfo::not_queried(
            "HID++ paired-device query failed to open /dev/hidraw2: No such file or directory (os error 2)",
        );

        let logical = logical_devices(&[endpoint]);
        let row = device_row_from_logical(&logical[0]);

        assert_eq!(row.access_state, DeviceAccessState::HidDeviceMissing);
    }

    #[test]
    fn logical_device_distinguishes_read_only_unresponsive_and_ready_states() {
        let mut endpoint = make_device(
            "/dev/hidraw2",
            "usb-0000:06:00.0-3/input2",
            CapabilityState::Supported,
        );
        endpoint.access.hidraw_readable = true;

        let logical = logical_devices(&[endpoint.clone()]);
        let row = device_row_from_logical(&logical[0]);
        assert_eq!(row.access_state, DeviceAccessState::ReadOnly);

        endpoint.access.hidraw_readwrite = true;
        endpoint.access.write_policy = WritePolicy::ExplicitApplyOnly;
        let logical = logical_devices(&[endpoint.clone()]);
        let row = device_row_from_logical(&logical[0]);
        assert_eq!(row.access_state, DeviceAccessState::NoPairedResponse);
        let discovery_row = device_row_from_logical_with_discovery(&logical[0], true);
        assert_eq!(
            discovery_row.access_state,
            DeviceAccessState::NoPairedResponse
        );
        assert_eq!(
            discovery_row.discovery_state,
            DeviceDiscoveryState::Enriching
        );

        endpoint.paired_device = Some(PairedDeviceInfo {
            slot: 1,
            name: Some("MX Master 3S".to_owned()),
            kind: Some("mouse".to_owned()),
            wpid: Some("B034".to_owned()),
            protocol: Some(HidppProtocolVersion { major: 4, minor: 5 }),
            unit_id: Some("AABBCCDD".to_owned()),
            model_id: Some("B03400000000".to_owned()),
            feature_count: 0,
            features: Vec::new(),
        });
        let logical = logical_devices(&[endpoint]);
        let row = device_row_from_logical(&logical[0]);
        assert_eq!(row.name, "MX Master 3S");
        assert_eq!(row.protocol, "4.5");
        assert_eq!(row.access_state, DeviceAccessState::Ready);
    }

    #[test]
    fn device_write_readiness_requires_verified_master3s_identity() {
        let mut endpoint =
            make_paired_device("/dev/hidraw2", "usb-0000:06:00.0-3/input2", "AABBCCDD");
        let paired = endpoint.paired_device.as_mut().unwrap();
        paired.name = Some("MX Anywhere 3S".to_owned());
        paired.wpid = Some("B037".to_owned());
        paired.model_id = Some("B03700000000".to_owned());

        let logical = logical_devices(&[endpoint]);
        let row = device_row_from_logical(&logical[0]);

        assert!(row.is_mouse);
        assert_eq!(row.access_state, DeviceAccessState::Unsupported);
    }

    #[test]
    fn logical_device_name_uses_master3s_model_id_when_name_is_missing() {
        let mut endpoint = make_device(
            "/dev/hidraw2",
            "usb-0000:06:00.0-3/input2",
            CapabilityState::Supported,
        );
        endpoint.paired_device = Some(PairedDeviceInfo {
            slot: 1,
            name: None,
            kind: Some("mouse".to_owned()),
            wpid: None,
            protocol: Some(HidppProtocolVersion { major: 4, minor: 5 }),
            unit_id: Some("AABBCCDD".to_owned()),
            model_id: Some("B03400000000".to_owned()),
            feature_count: 0,
            features: Vec::new(),
        });

        let logical = logical_devices(&[endpoint]);

        assert_eq!(logical_device_name(&logical[0]), "MX Master 3S");
        assert_eq!(device_row_from_logical(&logical[0]).name, "MX Master 3S");
    }

    #[test]
    fn logical_device_name_uses_receiver_wpid_when_feature_name_is_missing() {
        let mut endpoint = make_device(
            "/dev/hidraw2",
            "usb-0000:06:00.0-3/input2",
            CapabilityState::Supported,
        );
        endpoint.paired_device = Some(PairedDeviceInfo {
            slot: 1,
            name: None,
            kind: Some("mouse".to_owned()),
            wpid: Some("B034".to_owned()),
            protocol: None,
            unit_id: None,
            model_id: None,
            feature_count: 0,
            features: Vec::new(),
        });

        let logical = logical_devices(&[endpoint]);

        assert_eq!(logical_device_name(&logical[0]), "MX Master 3S");
        assert_eq!(device_row_from_logical(&logical[0]).name, "MX Master 3S");
    }

    #[test]
    fn maps_settings_to_widget_indices() {
        assert_eq!(wheel_mode_index(WheelRatchetMode::Ratchet), 0);
        assert_eq!(wheel_mode_index(WheelRatchetMode::FreeSpin), 1);
        assert_eq!(wheel_mode_index(WheelRatchetMode::SmartShift), 2);
        assert_eq!(thumb_wheel_index(ThumbWheelMode::HorizontalScroll), 0);
        assert_eq!(thumb_wheel_index(ThumbWheelMode::Disabled), 4);
    }

    #[test]
    fn maps_button_actions_to_widget_indices() {
        for (index, action) in ButtonAction::ALL.into_iter().enumerate() {
            assert_eq!(action_from_index(index as i32), action);
            assert_eq!(action_index(action), index as i32);
        }

        assert_eq!(action_from_index(99), ButtonAction::Native);
    }

    #[test]
    fn device_session_keeps_drafts_isolated_while_switching() {
        let first = Master3sSettings {
            pointer_speed_percent: 90,
            ..Master3sSettings::default()
        };
        let second = Master3sSettings {
            pointer_speed_percent: 130,
            ..Master3sSettings::default()
        };
        let devices = logical_devices(&[
            make_device(
                "/dev/hidraw2",
                "usb-0000:06:00.0-3/input2",
                CapabilityState::Supported,
            ),
            make_device(
                "/dev/hidraw3",
                "usb-0000:06:00.0-4/input2",
                CapabilityState::Supported,
            ),
        ]);
        let mut session =
            DeviceUiSession::new(&devices, vec![first, second], Master3sSettings::default());

        let mut first_draft = session.current().clone();
        first_draft.pointer_speed_percent = 105;
        session.replace_current(first_draft);
        assert!(session.select(1));
        assert_eq!(session.current().pointer_speed_percent, 130);
        assert!(!session.current_dirty());

        let mut second_draft = session.current().clone();
        second_draft.pointer_speed_percent = 145;
        session.replace_current(second_draft);
        assert!(session.select(0));
        assert_eq!(session.current().pointer_speed_percent, 105);
        assert!(session.current_dirty());
        assert!(session.select(1));
        assert_eq!(session.current().pointer_speed_percent, 145);
        assert!(session.current_dirty());
        assert!(!session.select(2));
        assert_eq!(session.current().pointer_speed_percent, 145);
    }

    #[test]
    fn device_session_dirty_state_tracks_the_saved_value() {
        let devices = logical_devices(&[make_device(
            "/dev/hidraw2",
            "usb-0000:06:00.0-3/input2",
            CapabilityState::Supported,
        )]);
        let saved = Master3sSettings::default();
        let mut session = DeviceUiSession::new(&devices, vec![saved.clone()], saved.clone());

        session.replace_current(Master3sSettings {
            pointer_speed_percent: 125,
            ..saved.clone()
        });
        assert!(session.current_dirty());

        session.replace_current(saved);
        assert!(!session.current_dirty());
    }

    #[test]
    fn device_session_can_save_all_dirty_drafts_before_restart() {
        let devices = logical_devices(&[
            make_paired_device("/dev/hidraw2", "usb-0000:06:00.0-3/input2", "AABBCCDD"),
            make_paired_device("/dev/hidraw3", "usb-0000:06:00.0-4/input2", "EEFF0011"),
        ]);
        let saved = Master3sSettings::default();
        let mut session = DeviceUiSession::new(&devices, vec![saved.clone(), saved.clone()], saved);

        let mut first = session.current().clone();
        first.pointer_speed_percent = 110;
        session.replace_current(first);
        assert!(session.select(1));
        let mut second = session.current().clone();
        second.pointer_speed_percent = 140;
        session.replace_current(second);

        let dirty = session.dirty_settings(&devices);
        assert_eq!(dirty.len(), 2);
        assert!(dirty.iter().all(|(settings_id, _)| settings_id.is_some()));
        session.mark_all_saved();
        assert!(!session.any_dirty());
        assert!(session.dirty_settings(&devices).is_empty());
    }

    #[test]
    fn device_session_preserves_drafts_and_selection_after_reorder() {
        let device_a = make_paired_device("/dev/hidraw2", "usb-0000:06:00.0-3/input2", "AABBCCDD");
        let device_b = make_paired_device("/dev/hidraw3", "usb-0000:06:00.0-4/input2", "11223344");
        let current = logical_devices(&[device_a.clone(), device_b.clone()]);
        let mut session = DeviceUiSession::new(
            &current,
            vec![settings_with_speed(105), settings_with_speed(145)],
            Master3sSettings::default(),
        );
        assert!(session.select(1));

        let next = logical_devices(&[device_b, device_a]);
        session.reconcile_devices(&next, None).unwrap();

        assert_eq!(session.selected_index, Some(0));
        assert_eq!(session.current().pointer_speed_percent, 145);
        assert_eq!(session.drafts[0].settings.pointer_speed_percent, 145);
        assert_eq!(session.drafts[1].settings.pointer_speed_percent, 105);
    }

    #[test]
    fn device_session_restores_detached_unit_on_another_transport() {
        let current = logical_devices(&[make_paired_device(
            "/dev/hidraw2",
            "usb-0000:06:00.0-3/input2",
            "AABBCCDD",
        )]);
        let mut session = DeviceUiSession::new(
            &current,
            vec![settings_with_speed(100)],
            Master3sSettings::default(),
        );
        session.replace_current(settings_with_speed(133));

        session.reconcile_devices(&[], None).unwrap();
        assert_eq!(session.selected_index, None);

        let reconnected = logical_devices(&[make_paired_device(
            "/dev/hidraw8",
            "usb-0000:06:00.0-9/input2",
            "aabbccdd",
        )]);
        session.reconcile_devices(&reconnected, None).unwrap();

        assert_eq!(session.selected_index, Some(0));
        assert_eq!(session.current().pointer_speed_percent, 133);
        assert!(session.current_dirty());
    }

    #[test]
    fn device_session_migrates_unknown_receiver_draft_when_mouse_is_identified() {
        let current = logical_devices(&[make_device(
            "/dev/hidraw2",
            "usb-0000:06:00.0-3/input2",
            CapabilityState::Supported,
        )]);
        let mut session = DeviceUiSession::new(
            &current,
            vec![settings_with_speed(121)],
            Master3sSettings::default(),
        );
        let next = logical_devices(&[make_paired_device(
            "/dev/hidraw2",
            "usb-0000:06:00.0-3/input2",
            "AABBCCDD",
        )]);
        let loads = Rc::new(Cell::new(0));
        let load_count = loads.clone();
        let loader: SettingsLoader = Rc::new(move |_| {
            load_count.set(load_count.get() + 1);
            Ok(settings_with_speed(87))
        });

        session.reconcile_devices(&next, Some(&loader)).unwrap();

        assert_eq!(loads.get(), 0);
        assert_eq!(session.current().pointer_speed_percent, 121);
        assert_eq!(
            session.drafts[0].strong_key.as_deref(),
            Some("046d:unit:AABBCCDD")
        );
    }

    #[test]
    fn device_session_does_not_move_draft_between_units_on_same_receiver() {
        let current = logical_devices(&[make_paired_device(
            "/dev/hidraw2",
            "usb-0000:06:00.0-3/input2",
            "AAAAAAAA",
        )]);
        let mut session = DeviceUiSession::new(
            &current,
            vec![settings_with_speed(157)],
            Master3sSettings::default(),
        );
        let next = logical_devices(&[make_paired_device(
            "/dev/hidraw2",
            "usb-0000:06:00.0-3/input2",
            "BBBBBBBB",
        )]);
        let loads = Rc::new(Cell::new(0));
        let load_count = loads.clone();
        let loader: SettingsLoader = Rc::new(move |settings_id| {
            assert_eq!(settings_id, "046d:unit:BBBBBBBB");
            load_count.set(load_count.get() + 1);
            Ok(settings_with_speed(87))
        });

        session.reconcile_devices(&next, Some(&loader)).unwrap();

        assert_eq!(loads.get(), 1);
        assert_eq!(session.current().pointer_speed_percent, 87);
    }

    #[test]
    fn device_session_reconcile_is_atomic_when_loading_a_new_device_fails() {
        let current = logical_devices(&[make_paired_device(
            "/dev/hidraw2",
            "usb-0000:06:00.0-3/input2",
            "AAAAAAAA",
        )]);
        let mut session = DeviceUiSession::new(
            &current,
            vec![settings_with_speed(117)],
            Master3sSettings::default(),
        );
        let next = logical_devices(&[
            make_paired_device("/dev/hidraw2", "usb-0000:06:00.0-3/input2", "AAAAAAAA"),
            make_paired_device("/dev/hidraw4", "usb-0000:06:00.0-5/input2", "BBBBBBBB"),
        ]);
        let loader: SettingsLoader =
            Rc::new(|_| Err(DogiError::Config("test loader failure".to_owned())));

        assert!(session.reconcile_devices(&next, Some(&loader)).is_err());
        assert_eq!(session.selected_index, Some(0));
        assert_eq!(session.drafts.len(), 1);
        assert_eq!(session.current().pointer_speed_percent, 117);
        assert!(session.detached_drafts.is_empty());
    }

    #[test]
    fn plan_scope_marks_device_and_local_steps() {
        let settings = Master3sSettings::default();
        let device_step = SettingsApplyStep {
            operation: SettingsApplyOperation::PointerSpeed { percent: 100 },
            feature: HidppFeature::PointerSpeed,
            requires_device_write: true,
        };
        let button_step = SettingsApplyStep {
            operation: SettingsApplyOperation::ButtonMapping {
                button: Master3sButton::Back,
                action: ButtonAction::Back,
            },
            feature: HidppFeature::ReprogrammableControls,
            requires_device_write: true,
        };
        let local_step = SettingsApplyStep {
            operation: SettingsApplyOperation::AppProfile {
                profile: AppProfile {
                    app_name: "Firefox".to_owned(),
                    pointer_speed_percent: 100,
                    thumb_wheel: ThumbWheelMode::HorizontalScroll,
                },
            },
            feature: HidppFeature::LocalProfile,
            requires_device_write: false,
        };

        assert_eq!(plan_step_scope(&device_step, &settings), PlanScope::Device);
        assert_eq!(plan_step_scope(&button_step, &settings), PlanScope::Device);
        assert_eq!(plan_step_scope(&local_step, &settings), PlanScope::Local);
    }

    #[test]
    fn plan_scope_marks_supported_thumb_wheel_modes_as_device_steps() {
        let thumb_step = SettingsApplyStep {
            operation: SettingsApplyOperation::ThumbWheel {
                mode: ThumbWheelMode::HorizontalScroll,
                speed_percent: dogi_core::DEFAULT_THUMB_WHEEL_SPEED_PERCENT,
            },
            feature: HidppFeature::ThumbWheel,
            requires_device_write: true,
        };
        let horizontal = Master3sSettings {
            thumb_wheel: ThumbWheelMode::HorizontalScroll,
            ..Master3sSettings::default()
        };
        let disabled = Master3sSettings {
            thumb_wheel: ThumbWheelMode::Disabled,
            ..Master3sSettings::default()
        };
        let zoom = Master3sSettings {
            thumb_wheel: ThumbWheelMode::Zoom,
            ..Master3sSettings::default()
        };

        assert_eq!(plan_step_scope(&thumb_step, &horizontal), PlanScope::Device);
        assert_eq!(plan_step_scope(&thumb_step, &disabled), PlanScope::Device);
        assert_eq!(plan_step_scope(&thumb_step, &zoom), PlanScope::Device);
    }

    #[test]
    fn dirty_apply_plan_contains_only_the_user_changes() {
        let saved = Master3sSettings::default();
        let target = Master3sSettings {
            pointer_speed_percent: 125,
            ..saved.clone()
        };

        let plan = device_apply_plan("device-1", &saved, &target);

        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].feature, HidppFeature::PointerSpeed);
    }

    #[test]
    fn clean_apply_plan_never_writes_saved_defaults_to_the_mouse() {
        let saved = Master3sSettings::default();

        let plan = device_apply_plan("device-1", &saved, &saved);

        assert!(plan.steps.is_empty());
    }

    #[test]
    fn smooth_scrolling_change_preserves_the_current_scroll_direction() {
        let saved = Master3sSettings::default();
        let target = Master3sSettings {
            high_resolution_scroll: false,
            ..saved.clone()
        };

        let plan = device_apply_plan("device-1", &saved, &target);
        let row = plan_row_from_step(&plan.steps[0], &target);

        assert!(matches!(
            plan.steps[0].operation,
            SettingsApplyOperation::ScrollBehavior {
                high_resolution: Some(false),
                natural: None,
            }
        ));
        assert!(row.primary_changed);
        assert!(!row.secondary_changed);
    }

    #[test]
    fn local_only_changes_do_not_create_mouse_write_steps() {
        let saved = Master3sSettings::default();
        let mut target = saved.clone();
        target.app_profiles.push(AppProfile {
            app_name: "Firefox".to_owned(),
            pointer_speed_percent: 90,
            thumb_wheel: ThumbWheelMode::Zoom,
        });

        assert!(
            device_apply_plan("device-1", &saved, &target)
                .steps
                .is_empty()
        );
    }

    #[test]
    fn apply_report_counts_remain_structured_for_translation() {
        let report = SettingsApplyReport {
            device_id: "device-1".to_owned(),
            profile_name: "Default".to_owned(),
            outcomes: vec![
                apply_outcome("pointer", SettingsApplyStatus::Applied),
                apply_outcome("wheel", SettingsApplyStatus::Applied),
                apply_outcome("thumb", SettingsApplyStatus::Unsupported),
            ],
        };

        let status = UiStatus::presentation(UiStatusKind::Warning, UiMessage::ApplySummary)
            .with_apply_counts(
                count_apply_status(&report, SettingsApplyStatus::Failed),
                count_apply_status(&report, SettingsApplyStatus::Applied),
                count_apply_status(&report, SettingsApplyStatus::Unsupported),
            );
        assert_eq!(status.failed, 0);
        assert_eq!(status.applied, 2);
        assert_eq!(status.unsupported, 1);
    }

    #[test]
    fn pending_apply_requires_same_device_and_settings() {
        let settings = Master3sSettings {
            pointer_speed_percent: 250,
            ..Master3sSettings::default()
        };
        let current = PendingApplyConfirmation::new(Some("device-1".to_owned()), settings.clone());
        let pending = Some(PendingApplyConfirmation::new(
            Some("device-1".to_owned()),
            settings,
        ));

        assert!(pending_apply_matches(&pending, &current));

        let different_device =
            PendingApplyConfirmation::new(Some("device-2".to_owned()), Master3sSettings::default());
        assert!(!pending_apply_matches(&pending, &different_device));

        let changed_settings = Master3sSettings {
            pointer_speed_percent: 125,
            ..Master3sSettings::default()
        };
        let changed = PendingApplyConfirmation::new(Some("device-1".to_owned()), changed_settings);
        assert!(!pending_apply_matches(&pending, &changed));
    }

    #[test]
    fn app_profile_upsert_adds_and_updates_by_normalized_name() {
        let mut settings = Master3sSettings {
            app_profiles: Vec::new(),
            ..Master3sSettings::default()
        };

        upsert_app_profile(&mut settings, " firefox ", 90, ThumbWheelMode::TabSwitch).unwrap();
        upsert_app_profile(&mut settings, "Firefox", 110, ThumbWheelMode::Zoom).unwrap();

        assert_eq!(settings.app_profiles.len(), 1);
        assert_eq!(settings.app_profiles[0].app_name, "Firefox");
        assert_eq!(settings.app_profiles[0].pointer_speed_percent, 110);
        assert_eq!(settings.app_profiles[0].thumb_wheel, ThumbWheelMode::Zoom);
    }

    #[test]
    fn app_profile_remove_deletes_by_normalized_name() {
        let mut settings = Master3sSettings {
            app_profiles: vec![AppProfile {
                app_name: "Visual Studio Code".to_owned(),
                pointer_speed_percent: 90,
                thumb_wheel: ThumbWheelMode::HorizontalScroll,
            }],
            ..Master3sSettings::default()
        };

        remove_app_profile(&mut settings, "visual-studio-code").unwrap();

        assert!(settings.app_profiles.is_empty());
    }

    #[test]
    fn app_profile_remove_reports_missing_profile() {
        let mut settings = Master3sSettings {
            app_profiles: Vec::new(),
            ..Master3sSettings::default()
        };

        let error = remove_app_profile(&mut settings, "firefox").unwrap_err();

        assert_eq!(error, ProfileEditError::NotFound("firefox".to_owned()));
    }

    #[test]
    fn user_discovery_publishes_inventory_before_enrichment() {
        let discovery =
            DeviceDiscovery::new(Arc::new(|| Ok(Vec::new())), Arc::new(|| Ok(Vec::new())));
        let (sender, receiver) = mpsc::channel();

        run_device_discovery(discovery, DeviceScanIntent::User, &sender);

        assert_eq!(receiver.recv().unwrap().phase, DeviceScanPhase::Inventory);
        assert_eq!(receiver.recv().unwrap().phase, DeviceScanPhase::Enriched);
    }

    #[test]
    fn hidpp_recovery_skips_the_unchanged_inventory_phase() {
        let discovery = DeviceDiscovery::new(
            Arc::new(|| panic!("inventory must not run during HID++ recovery")),
            Arc::new(|| Ok(Vec::new())),
        );
        let (sender, receiver) = mpsc::channel();

        run_device_discovery(discovery, DeviceScanIntent::HidppRecovery, &sender);

        assert_eq!(receiver.recv().unwrap().phase, DeviceScanPhase::Enriched);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn interactive_controls_handle_pointer_input_and_deferred_language_switching() {
        let runtime = SnapshotRuntime::builder()
            .clock_mode(ClockMode::Manual)
            .build()
            .unwrap();

        let window = MainWindow::new().unwrap();
        window.show().unwrap();
        {
            let tray = AppTray::new().unwrap();
            tray.set_enabled(true);
            assert!(tray.get_enabled());
            tray.set_enabled(false);
            assert!(!tray.get_enabled());
        }
        runtime.set_size(window.window(), (1260, 780), 1.0).unwrap();
        let device = preview_device_row(
            "device-1",
            "MX Master 3S",
            DeviceConnection::Bolt,
            DeviceAccessState::Ready,
            "/dev/hidraw2",
            85,
            true,
        );
        window.set_devices(Rc::new(slint::VecModel::from(vec![device.clone()])).into());
        window.set_selected_device(device);
        window.set_selected_device_index(0);
        window.set_runtime_state(DesktopRuntimeState::Running);
        window.set_app_profiles_supported(true);
        window.set_rescan_enabled(true);
        window.set_app_profile_rows(
            Rc::new(slint::VecModel::from(vec![
                AppProfileRow {
                    app: "Firefox".into(),
                    initial: "F".into(),
                    pointer_speed: 90,
                    thumb_wheel_index: 1,
                },
                AppProfileRow {
                    app: "Code".into(),
                    initial: "C".into(),
                    pointer_speed: 100,
                    thumb_wheel_index: 0,
                },
            ]))
            .into(),
        );

        let selected_device = Rc::new(Cell::new(-1));
        let selected_device_result = selected_device.clone();
        window.on_select_device(move |index| selected_device_result.set(index));
        let selected_profile = Rc::new(Cell::new(-1));
        let selected_profile_result = selected_profile.clone();
        window.on_select_app_profile(move |index| selected_profile_result.set(index));
        let reset_count = Rc::new(Cell::new(0));
        let reset_count_result = reset_count.clone();
        window.on_revert_current_device(move || reset_count_result.set(1));
        let rescan_count = Rc::new(Cell::new(0));
        let rescan_count_result = rescan_count.clone();
        window.on_rescan_devices(move || rescan_count_result.set(rescan_count_result.get() + 1));
        let cancel_count = Rc::new(Cell::new(0));
        let cancel_count_result = cancel_count.clone();
        window.on_cancel_apply(move || cancel_count_result.set(1));
        let apply_count = Rc::new(Cell::new(0));
        let apply_count_result = apply_count.clone();
        window.on_apply_requested(move || apply_count_result.set(apply_count_result.get() + 1));
        let save_count = Rc::new(Cell::new(0));
        let save_count_result = save_count.clone();
        window.on_save_for_later(move || save_count_result.set(save_count_result.get() + 1));
        let saved_language = Rc::new(Cell::new(None::<ApplicationLanguage>));
        let saved_language_result = saved_language.clone();
        let language_save: ApplicationPreferenceSaver = Rc::new(move |change| {
            if let ApplicationPreferenceChange::Language(language) = change {
                saved_language_result.set(Some(language));
            }
            Ok(())
        });
        let active_language = Rc::new(Cell::new(ApplicationLanguage::System));
        let language_window = window.as_weak();
        window.on_language_selected(move |index| {
            request_language_change(
                language_window.clone(),
                active_language.clone(),
                language_save.clone(),
                ApplicationLanguage::from_index(index),
            );
        });

        runtime.render(window.window()).unwrap();

        let click = |x, y| {
            window
                .window()
                .dispatch_event(slint::platform::WindowEvent::PointerPressed {
                    position: slint::LogicalPosition { x, y },
                    button: slint::platform::PointerEventButton::Left,
                });
            window
                .window()
                .dispatch_event(slint::platform::WindowEvent::PointerReleased {
                    position: slint::LogicalPosition { x, y },
                    button: slint::platform::PointerEventButton::Left,
                });
        };

        click(100.0, 160.0);
        assert_eq!(selected_device.get(), 0);

        window.set_pointer_speed(100.0);
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::PointerPressed {
                position: slint::LogicalPosition { x: 950.0, y: 203.0 },
                button: slint::platform::PointerEventButton::Left,
            });
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                position: slint::LogicalPosition {
                    x: 1100.0,
                    y: 203.0,
                },
            });
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::PointerReleased {
                position: slint::LogicalPosition {
                    x: 1100.0,
                    y: 203.0,
                },
                button: slint::platform::PointerEventButton::Left,
            });
        assert!(window.get_pointer_speed() > 100.0);

        for (x, expected_page) in [(484.0, 1), (632.0, 2), (336.0, 0)] {
            click(x, 88.0);
            assert_eq!(window.get_page_index(), expected_page);
        }

        let mut enriching_device = window.get_selected_device();
        enriching_device.discovery_state = DeviceDiscoveryState::Enriching;
        window.set_selected_device(enriching_device.clone());
        window.set_draft_dirty(true);
        runtime.render(window.window()).unwrap();
        click(484.0, 88.0);
        assert_eq!(window.get_page_index(), 0);
        click(1180.0, 32.0);
        assert!(!window.get_revert_confirm_visible());
        click(1160.0, 746.0);
        assert_eq!(save_count.get(), 0);
        enriching_device.discovery_state = DeviceDiscoveryState::Idle;
        window.set_selected_device(enriching_device);
        window.set_draft_dirty(false);

        click(100.0, 745.0);
        assert_eq!(window.get_page_index(), 3);
        click(100.0, 160.0);
        assert_eq!(window.get_page_index(), 0);

        click(484.0, 88.0);
        runtime.render(window.window()).unwrap();
        window.set_selected_button_index(2);
        click(1000.0, 407.0);
        assert_eq!(window.get_selected_button_index(), 0);

        click(632.0, 88.0);
        runtime.render(window.window()).unwrap();
        click(400.0, 330.0);
        assert_eq!(selected_profile.get(), 1);

        window.set_draft_dirty(true);
        click(1180.0, 32.0);
        assert!(window.get_revert_confirm_visible());
        assert_eq!(reset_count.get(), 0);
        click(760.0, 462.0);
        assert_eq!(reset_count.get(), 1);
        click(1070.0, 32.0);
        assert_eq!(rescan_count.get(), 1);
        window.set_rescan_in_progress(true);
        click(1070.0, 32.0);
        assert_eq!(rescan_count.get(), 1);
        window.set_rescan_in_progress(false);

        window.set_confirm_visible(true);
        let page_before_modal_click = window.get_page_index();
        click(336.0, 88.0);
        assert_eq!(window.get_page_index(), page_before_modal_click);
        click(740.0, 500.0);
        assert_eq!(cancel_count.get(), 1);

        window.set_confirm_visible(false);
        let mut device = window.get_selected_device();
        device.access_state = DeviceAccessState::ReadOnly;
        window.set_selected_device(device.clone());
        runtime.render(window.window()).unwrap();
        click(1160.0, 746.0);
        assert_eq!(apply_count.get(), 0);
        assert_eq!(save_count.get(), 1);
        window.set_draft_dirty(false);
        click(1160.0, 746.0);
        assert_eq!(save_count.get(), 1);
        device.access_state = DeviceAccessState::Ready;
        window.set_selected_device(device);
        click(1160.0, 746.0);
        assert_eq!(apply_count.get(), 0);
        window.set_draft_dirty(true);
        click(1160.0, 746.0);
        assert_eq!(apply_count.get(), 1);
        assert_eq!(save_count.get(), 1);

        window.set_page_index(0);
        window.set_thumb_wheel_index(0);
        window.set_horizontal_scroll_test_supported(true);
        window.set_horizontal_scroll_test_open(true);
        runtime.render(window.window()).unwrap();
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                position: slint::LogicalPosition { x: 900.0, y: 660.0 },
                delta_x: 48.0,
                delta_y: 0.0,
            });
        assert!(window.get_horizontal_scroll_test_moved());

        window.set_page_index(3);
        window.set_language_index(0);
        runtime.render(window.window()).unwrap();
        click(820.0, 150.0);
        runtime.render(window.window()).unwrap();
        click(820.0, 261.0);
        slint::platform::update_timers_and_animations();
        assert_eq!(window.get_language_index(), 2);
        assert_eq!(
            saved_language.get(),
            Some(ApplicationLanguage::SimplifiedChinese)
        );
        slint::select_bundled_translation("en").unwrap();
        window.set_language_index(0);
        runtime.render(window.window()).unwrap();
        click(820.0, 150.0);
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: slint::platform::Key::Escape.into(),
            });
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::KeyReleased {
                text: slint::platform::Key::Escape.into(),
            });
    }

    #[test]
    fn render_preview_snapshot_when_requested() {
        let Some(path) = std::env::var_os("DOGI_UI_SNAPSHOT") else {
            return;
        };
        let runtime = SnapshotRuntime::builder()
            .clock_mode(ClockMode::Manual)
            .build()
            .unwrap();

        let window = MainWindow::new().unwrap();
        if let Ok(locale) = std::env::var("DOGI_UI_SNAPSHOT_LOCALE") {
            slint::select_bundled_translation(&locale).unwrap();
            window.set_language_index(if locale == "zh_CN" { 2 } else { 1 });
        }
        window.set_high_contrast(std::env::var_os("DOGI_UI_SNAPSHOT_HIGH_CONTRAST").is_some());
        window.show().unwrap();
        let snapshot_width = std::env::var("DOGI_UI_SNAPSHOT_WIDTH")
            .ok()
            .and_then(|width| width.parse::<u32>().ok())
            .unwrap_or(1260);
        let snapshot_height = std::env::var("DOGI_UI_SNAPSHOT_HEIGHT")
            .ok()
            .and_then(|height| height.parse::<u32>().ok())
            .unwrap_or(780);
        runtime
            .set_size(window.window(), (snapshot_width, snapshot_height), 1.0)
            .unwrap();
        let empty_device_preview = std::env::var_os("DOGI_UI_SNAPSHOT_EMPTY").is_some();
        let multi_device_preview = std::env::var_os("DOGI_UI_SNAPSHOT_MULTI").is_some();
        let empty_profiles_preview = std::env::var_os("DOGI_UI_SNAPSHOT_EMPTY_PROFILES").is_some();
        let stress_preview = std::env::var_os("DOGI_UI_SNAPSHOT_STRESS").is_some();
        let write_ready_preview = std::env::var_os("DOGI_UI_SNAPSHOT_WRITE_READY").is_some();
        let diff_only_preview = std::env::var_os("DOGI_UI_SNAPSHOT_DIFF_ONLY").is_some();
        let discovering_preview = std::env::var_os("DOGI_UI_SNAPSHOT_DISCOVERING").is_some();
        let access_preview = std::env::var("DOGI_UI_SNAPSHOT_ACCESS")
            .ok()
            .map(|state| match state.as_str() {
                "missing" => DeviceAccessState::HidDeviceMissing,
                "permission" => DeviceAccessState::HidPermissionDenied,
                "read-only" => DeviceAccessState::ReadOnly,
                "no-response" => DeviceAccessState::NoPairedResponse,
                "unsupported" => DeviceAccessState::Unsupported,
                "hidpp" => DeviceAccessState::HidppUnavailable,
                other => panic!("unknown DOGI_UI_SNAPSHOT_ACCESS state: {other}"),
            })
            .or_else(|| {
                std::env::var_os("DOGI_UI_SNAPSHOT_INACCESSIBLE")
                    .is_some()
                    .then_some(DeviceAccessState::HidDeviceMissing)
            });
        let mut device_rows = if empty_device_preview {
            Vec::new()
        } else if let Some(access_state) = access_preview {
            let mut row = preview_device_row(
                "bolt-receiver",
                "Logi Bolt receiver",
                DeviceConnection::Bolt,
                access_state,
                "/dev/hidraw2",
                -1,
                false,
            );
            row.interface_count = 4;
            vec![row]
        } else if stress_preview {
            (0..12)
                .map(|index| {
                    preview_device_row(
                        format!("stress-device-{index}"),
                        if index == 0 {
                            "MX Master 3S — Design Studio Workstation".to_owned()
                        } else {
                            format!("MX Anywhere 3S · Workspace {}", index + 1)
                        },
                        if index % 2 == 0 {
                            DeviceConnection::Bluetooth
                        } else {
                            DeviceConnection::Bolt
                        },
                        DeviceAccessState::Ready,
                        format!("/dev/hidraw{}", index + 2),
                        96 - index * 4,
                        true,
                    )
                })
                .collect()
        } else {
            let mut rows = vec![preview_device_row(
                "bolt-mx-master-3s",
                "MX Master 3S",
                DeviceConnection::Bolt,
                DeviceAccessState::Ready,
                "/dev/hidraw2",
                85,
                true,
            )];
            if multi_device_preview {
                rows.push(preview_device_row(
                    "bluetooth-mx-anywhere-3s",
                    "MX Anywhere 3S",
                    DeviceConnection::Bluetooth,
                    DeviceAccessState::Ready,
                    "/dev/hidraw6",
                    64,
                    true,
                ));
            }
            rows
        };
        if discovering_preview && let Some(row) = device_rows.first_mut() {
            row.name = "Logi Bolt receiver".into();
            row.access_state = DeviceAccessState::NoPairedResponse;
            row.discovery_state = DeviceDiscoveryState::Enriching;
            row.battery_level = -1;
            row.interface_count = 4;
            row.is_mouse = false;
        }
        let selected_device_index = if empty_device_preview { -1 } else { 0 };
        let selected_device = device_rows.first().cloned().unwrap_or_default();
        window.set_devices(Rc::new(slint::VecModel::from(device_rows)).into());
        window.set_selected_device(selected_device);
        window.set_selected_device_index(selected_device_index);
        window.set_runtime_management_supported(true);
        if std::env::var_os("DOGI_UI_SNAPSHOT_RUNTIME_OFF").is_some() {
            window.set_runtime_enabled(false);
            window.set_runtime_state(DesktopRuntimeState::Stopped);
        } else if let Some(detail) = std::env::var_os("DOGI_UI_SNAPSHOT_RUNTIME_ERROR") {
            window.set_runtime_enabled(true);
            window.set_runtime_state(DesktopRuntimeState::Degraded);
            window.set_runtime_detail(detail.to_string_lossy().into_owned().into());
        } else {
            window.set_runtime_enabled(true);
            window.set_runtime_state(DesktopRuntimeState::Running);
        }
        window.set_update_supported(true);
        window.set_current_version(env!("CARGO_PKG_VERSION").into());
        if let Some(version) = std::env::var_os("DOGI_UI_SNAPSHOT_UPDATE_READY") {
            window.set_available_version(version.to_string_lossy().into_owned().into());
            window.set_update_state(UpdateState::Ready);
        } else {
            window.set_update_state(UpdateState::Current);
        }
        window.set_horizontal_scroll_test_supported(true);
        window.set_app_profiles_supported(std::env::var_os("DOGI_UI_SNAPSHOT_WAYLAND").is_none());
        window.set_rescan_enabled(true);
        let app_profile_rows = if empty_profiles_preview || write_ready_preview {
            Vec::new()
        } else if stress_preview {
            (0..14)
                .map(|index| AppProfileRow {
                    app: if index == 0 {
                        "Firefox Developer Edition — Research Workspace".into()
                    } else {
                        format!("Creative application workspace {}", index + 1).into()
                    },
                    initial: if index == 0 { "F" } else { "A" }.into(),
                    pointer_speed: 80 + index * 5,
                    thumb_wheel_index: 0,
                })
                .collect()
        } else {
            vec![
                AppProfileRow {
                    app: "Firefox".into(),
                    initial: "F".into(),
                    pointer_speed: 90,
                    thumb_wheel_index: 1,
                },
                AppProfileRow {
                    app: "Code".into(),
                    initial: "C".into(),
                    pointer_speed: 100,
                    thumb_wheel_index: 0,
                },
                AppProfileRow {
                    app: "Figma".into(),
                    initial: "F".into(),
                    pointer_speed: 120,
                    thumb_wheel_index: 2,
                },
            ]
        };
        window.set_app_profile_rows(Rc::new(slint::VecModel::from(app_profile_rows)).into());
        let plan_rows = if diff_only_preview {
            vec![PlanRow {
                kind: PlanKind::PointerSpeed,
                scope: PlanScope::Device,
                value: 120,
                ..PlanRow::default()
            }]
        } else if write_ready_preview {
            vec![
                PlanRow {
                    kind: PlanKind::PointerSpeed,
                    scope: PlanScope::Device,
                    value: 120,
                    ..PlanRow::default()
                },
                PlanRow {
                    kind: PlanKind::WheelBehavior,
                    primary_index: 2,
                    scope: PlanScope::Device,
                    value: 45,
                    enabled: true,
                    ..PlanRow::default()
                },
                PlanRow {
                    kind: PlanKind::ThumbWheel,
                    primary_index: 0,
                    scope: PlanScope::Device,
                    value: 140,
                    ..PlanRow::default()
                },
            ]
        } else if stress_preview {
            (0..12)
                .map(|index| PlanRow {
                    kind: if index % 3 == 0 {
                        PlanKind::AppProfile
                    } else {
                        PlanKind::PointerSpeed
                    },
                    scope: if index % 3 == 0 {
                        PlanScope::Local
                    } else {
                        PlanScope::Device
                    },
                    name: format!("Workspace {}", index + 1).into(),
                    value: 80 + index * 5,
                    ..PlanRow::default()
                })
                .collect()
        } else {
            vec![
                PlanRow {
                    kind: PlanKind::PointerSpeed,
                    scope: PlanScope::Device,
                    value: 120,
                    ..PlanRow::default()
                },
                PlanRow {
                    kind: PlanKind::WheelBehavior,
                    primary_index: 2,
                    scope: PlanScope::Device,
                    value: 45,
                    enabled: true,
                    ..PlanRow::default()
                },
                PlanRow {
                    kind: PlanKind::ScrollBehavior,
                    scope: PlanScope::Device,
                    enabled: true,
                    primary_changed: true,
                    secondary_changed: true,
                    ..PlanRow::default()
                },
                PlanRow {
                    kind: PlanKind::ThumbWheel,
                    scope: PlanScope::Device,
                    primary_index: 2,
                    ..PlanRow::default()
                },
            ]
        };
        let plan_row_count = plan_rows.len();
        window.set_plan_rows(Rc::new(slint::VecModel::from(plan_rows)).into());
        set_window_status(&window, UiStatus::default());
        window.set_pointer_speed(120.0);
        window.set_smart_shift_threshold(45.0);
        window.set_high_resolution_scroll(true);
        window.set_scroll_direction_index(0);
        window.set_wheel_mode_index(2);
        window.set_thumb_wheel_index(if write_ready_preview { 0 } else { 2 });
        window.set_thumb_wheel_speed(if write_ready_preview { 140.0 } else { 100.0 });
        if std::env::var_os("DOGI_UI_SNAPSHOT_SCROLL_TEST").is_some() {
            window.set_horizontal_scroll_test_open(true);
            window.set_horizontal_scroll_test_active(true);
            window.set_horizontal_scroll_test_mode_index(1);
        }
        window.set_back_action_index(if write_ready_preview { 10 } else { 0 });
        window.set_forward_action_index(if write_ready_preview { 10 } else { 1 });
        window.set_gesture_action_index(if write_ready_preview { 10 } else { 7 });
        window.set_mode_shift_action_index(if write_ready_preview { 10 } else { 5 });
        window.set_middle_action_index(if write_ready_preview { 10 } else { 6 });
        window.set_app_profile_editor_app(
            if empty_profiles_preview {
                ""
            } else if stress_preview {
                "Firefox Developer Edition — Research Workspace"
            } else {
                "Firefox"
            }
            .into(),
        );
        window.set_app_profile_editor_pointer_speed(if empty_profiles_preview {
            100.0
        } else {
            90.0
        });
        window.set_app_profile_editor_thumb_wheel_index(if empty_profiles_preview { 0 } else { 1 });
        window.set_selected_button_index(2);
        window.set_selected_app_profile_index(if empty_profiles_preview { -1 } else { 0 });
        window.set_draft_dirty(std::env::var_os("DOGI_UI_SNAPSHOT_CLEAN").is_none());
        if let Ok(theme_index) = std::env::var("DOGI_UI_SNAPSHOT_THEME")
            && let Ok(theme_index) = theme_index.parse::<i32>()
        {
            window.set_theme_index(theme_index.clamp(0, 2));
        }
        if let Ok(page_index) = std::env::var("DOGI_UI_SNAPSHOT_PAGE")
            && let Ok(page_index) = page_index.parse::<i32>()
        {
            window.set_page_index(page_index.clamp(0, 3));
        }
        if std::env::var_os("DOGI_UI_SNAPSHOT_CONFIRM").is_some() {
            window.set_confirm_visible(true);
            window.set_confirm_change_count(i32::try_from(plan_row_count).unwrap_or(i32::MAX));
        }
        if std::env::var_os("DOGI_UI_SNAPSHOT_UPDATE_CONFIRM").is_some() {
            window.set_page_index(3);
            window.set_update_install_confirm_visible(true);
        }
        if let Ok(focus_steps) = std::env::var("DOGI_UI_SNAPSHOT_FOCUS_STEPS")
            && let Ok(focus_steps) = focus_steps.parse::<usize>()
        {
            for _ in 0..focus_steps {
                window
                    .window()
                    .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                        text: slint::platform::Key::Tab.into(),
                    });
                window
                    .window()
                    .dispatch_event(slint::platform::WindowEvent::KeyReleased {
                        text: slint::platform::Key::Tab.into(),
                    });
            }
        }
        if std::env::var_os("DOGI_UI_SNAPSHOT_LANGUAGE_MENU").is_some() {
            window.set_page_index(3);
            runtime.render(window.window()).unwrap();
            for event in [
                slint::platform::WindowEvent::PointerPressed {
                    position: slint::LogicalPosition { x: 820.0, y: 156.0 },
                    button: slint::platform::PointerEventButton::Left,
                },
                slint::platform::WindowEvent::PointerReleased {
                    position: slint::LogicalPosition { x: 820.0, y: 156.0 },
                    button: slint::platform::PointerEventButton::Left,
                },
            ] {
                window.window().dispatch_event(event);
            }
        }

        runtime
            .render(window.window())
            .unwrap()
            .write_png(Path::new(&path))
            .unwrap();
    }

    fn preview_device_row(
        id: impl Into<slint::SharedString>,
        name: impl Into<slint::SharedString>,
        connection: DeviceConnection,
        access_state: DeviceAccessState,
        path: impl Into<slint::SharedString>,
        battery_level: i32,
        is_mouse: bool,
    ) -> DeviceRow {
        DeviceRow {
            id: id.into(),
            name: name.into(),
            path: path.into(),
            protocol: "4.5".into(),
            connection,
            access_state,
            discovery_state: DeviceDiscoveryState::Idle,
            battery_level,
            battery_state: BatteryState::Discharging,
            interface_count: 1,
            is_mouse,
        }
    }

    fn make_device(path: &str, physical_path: &str, hidpp: CapabilityState) -> DeviceInfo {
        DeviceInfo {
            id: path.to_owned(),
            name: "Logitech USB Receiver".to_owned(),
            paired_device: None,
            manufacturer: Some("Logitech".to_owned()),
            serial_number: None,
            bus: BusKind::Usb,
            bus_id: Some(0x0003),
            vendor_id: 0x046d,
            product_id: 0xc548,
            release_number: Some(0x0503),
            connection: ConnectionKind::Bolt,
            receiver_kind: Some(ReceiverKind::Bolt),
            path: path.to_owned(),
            sysfs_path: format!("/sys/class/hidraw/{}", path.trim_start_matches("/dev/")),
            physical_path: Some(physical_path.to_owned()),
            driver: Some("hid-generic".to_owned()),
            interface_number: None,
            usage_page: None,
            usage: None,
            access: DeviceAccess {
                sysfs_readable: true,
                hidraw_readable: false,
                hidraw_readwrite: false,
                write_policy: WritePolicy::Disabled,
            },
            battery: dogi_core::BatteryInfo::not_queried("battery requires HID++ query"),
            report_descriptor: ReportDescriptorInfo::default(),
            capabilities: DeviceCapabilities {
                hidpp,
                ..DeviceCapabilities::default()
            },
        }
    }

    fn make_paired_device(path: &str, physical_path: &str, unit_id: &str) -> DeviceInfo {
        let mut device = make_device(path, physical_path, CapabilityState::Supported);
        device.access.hidraw_readable = true;
        device.access.hidraw_readwrite = true;
        device.access.write_policy = WritePolicy::ExplicitApplyOnly;
        device.paired_device = Some(PairedDeviceInfo {
            slot: 1,
            name: Some("MX Master 3S".to_owned()),
            kind: Some("mouse".to_owned()),
            wpid: Some("B034".to_owned()),
            protocol: Some(HidppProtocolVersion { major: 4, minor: 5 }),
            unit_id: Some(unit_id.to_owned()),
            model_id: Some("B03400000000".to_owned()),
            feature_count: 0,
            features: Vec::new(),
        });
        device
    }

    fn settings_with_speed(pointer_speed_percent: u8) -> Master3sSettings {
        Master3sSettings {
            pointer_speed_percent,
            ..Master3sSettings::default()
        }
    }

    fn apply_outcome(title: &str, status: SettingsApplyStatus) -> SettingsApplyOutcome {
        SettingsApplyOutcome {
            title: title.to_owned(),
            feature: HidppFeature::PointerSpeed,
            status,
            detail: None,
        }
    }
}
