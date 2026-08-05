use serde::{Deserialize, Serialize};

pub const DEFAULT_THUMB_WHEEL_SPEED_PERCENT: u16 = 100;
pub const MIN_THUMB_WHEEL_SPEED_PERCENT: u16 = 25;
pub const MAX_THUMB_WHEEL_SPEED_PERCENT: u16 = 400;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Master3sSettings {
    pub profile_name: String,
    pub pointer_speed_percent: u8,
    pub smart_shift_enabled: bool,
    pub smart_shift_threshold: u8,
    pub ratchet_mode: WheelRatchetMode,
    pub high_resolution_scroll: bool,
    pub natural_scroll: bool,
    pub thumb_wheel: ThumbWheelMode,
    #[serde(default = "default_thumb_wheel_speed_percent")]
    pub thumb_wheel_speed_percent: u16,
    pub buttons: Vec<ButtonBinding>,
    pub app_profiles: Vec<AppProfile>,
}

impl Default for Master3sSettings {
    fn default() -> Self {
        Self {
            profile_name: "Default".to_owned(),
            pointer_speed_percent: 100,
            smart_shift_enabled: true,
            smart_shift_threshold: 45,
            ratchet_mode: WheelRatchetMode::SmartShift,
            high_resolution_scroll: true,
            natural_scroll: false,
            thumb_wheel: ThumbWheelMode::HorizontalScroll,
            thumb_wheel_speed_percent: DEFAULT_THUMB_WHEEL_SPEED_PERCENT,
            buttons: default_master3s_buttons(),
            app_profiles: Vec::new(),
        }
    }
}

impl Master3sSettings {
    pub fn normalized(&self) -> Self {
        let mut settings = self.clone();
        settings.profile_name = non_empty_or_default(&settings.profile_name, "Default");
        settings.pointer_speed_percent = clamp_u8(settings.pointer_speed_percent, 50, 200);
        settings.smart_shift_threshold = clamp_u8(settings.smart_shift_threshold, 1, 50);
        settings.thumb_wheel_speed_percent = settings
            .thumb_wheel_speed_percent
            .clamp(MIN_THUMB_WHEEL_SPEED_PERCENT, MAX_THUMB_WHEEL_SPEED_PERCENT);
        if !settings.smart_shift_enabled && settings.ratchet_mode == WheelRatchetMode::SmartShift {
            settings.ratchet_mode = WheelRatchetMode::Ratchet;
        }
        settings.smart_shift_enabled = settings.ratchet_mode == WheelRatchetMode::SmartShift;
        settings.buttons = normalize_button_bindings(&settings.buttons);

        for profile in &mut settings.app_profiles {
            profile.app_name = non_empty_or_default(&profile.app_name, "Application");
            profile.pointer_speed_percent = clamp_u8(profile.pointer_speed_percent, 50, 200);
        }

        settings
    }

    pub fn button_action(&self, button: Master3sButton) -> ButtonAction {
        self.buttons
            .iter()
            .find(|binding| binding.button == button)
            .map(|binding| binding.action)
            .unwrap_or_else(|| default_button_action(button))
    }

    pub fn set_button_action(&mut self, button: Master3sButton, action: ButtonAction) {
        if let Some(binding) = self
            .buttons
            .iter_mut()
            .find(|binding| binding.button == button)
        {
            binding.action = action;
        } else {
            self.buttons.push(ButtonBinding { button, action });
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WheelRatchetMode {
    Ratchet,
    FreeSpin,
    SmartShift,
}

impl WheelRatchetMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ratchet => "Ratchet",
            Self::FreeSpin => "Free spin",
            Self::SmartShift => "SmartShift",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThumbWheelMode {
    HorizontalScroll,
    TabSwitch,
    Zoom,
    Volume,
    Disabled,
}

impl ThumbWheelMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::HorizontalScroll => "Horizontal scroll",
            Self::TabSwitch => "Tab switch",
            Self::Zoom => "Zoom",
            Self::Volume => "Volume",
            Self::Disabled => "Disabled",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ButtonBinding {
    pub button: Master3sButton,
    pub action: ButtonAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Master3sButton {
    Back,
    Forward,
    Gesture,
    ModeShift,
    Middle,
}

impl Master3sButton {
    pub const ALL: [Self; 5] = [
        Self::Back,
        Self::Forward,
        Self::Gesture,
        Self::ModeShift,
        Self::Middle,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Back => "Back button",
            Self::Forward => "Forward button",
            Self::Gesture => "Gesture button",
            Self::ModeShift => "Mode shift",
            Self::Middle => "Middle click",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ButtonAction {
    Native,
    Back,
    Forward,
    #[serde(alias = "mission_control")]
    Overview,
    #[serde(alias = "app_expose")]
    WindowSwitcher,
    #[serde(alias = "desktop_left")]
    PreviousWorkspace,
    #[serde(alias = "desktop_right")]
    NextWorkspace,
    MiddleClick,
    Copy,
    Paste,
    Disabled,
}

impl ButtonAction {
    pub const ALL: [Self; 11] = [
        Self::Back,
        Self::Forward,
        Self::Overview,
        Self::WindowSwitcher,
        Self::PreviousWorkspace,
        Self::NextWorkspace,
        Self::MiddleClick,
        Self::Copy,
        Self::Paste,
        Self::Disabled,
        Self::Native,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Native => "Default behavior",
            Self::Back => "Back",
            Self::Forward => "Forward",
            Self::Overview => "Activities overview",
            Self::WindowSwitcher => "Switch windows",
            Self::PreviousWorkspace => "Previous workspace",
            Self::NextWorkspace => "Next workspace",
            Self::MiddleClick => "Middle click",
            Self::Copy => "Copy",
            Self::Paste => "Paste",
            Self::Disabled => "Disabled",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppProfile {
    pub app_name: String,
    pub pointer_speed_percent: u8,
    pub thumb_wheel: ThumbWheelMode,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActiveApplication {
    pub name: Option<String>,
    pub class: Option<String>,
    pub executable: Option<String>,
}

impl ActiveApplication {
    pub fn new(name: Option<String>, class: Option<String>, executable: Option<String>) -> Self {
        Self {
            name: clean_optional_app_string(name),
            class: clean_optional_app_string(class),
            executable: clean_optional_app_string(executable),
        }
    }

    pub fn summary(&self) -> String {
        [
            self.name.as_deref(),
            self.class.as_deref(),
            self.executable.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" / ")
    }

    fn match_fields(&self) -> impl Iterator<Item = &str> {
        [
            self.name.as_deref(),
            self.class.as_deref(),
            self.executable.as_deref(),
        ]
        .into_iter()
        .flatten()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EffectiveAppSettings {
    pub settings: Master3sSettings,
    pub matched_profile: Option<AppProfile>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LocalRuntimePlan {
    pub profile_name: String,
    pub thumb_wheel: Option<ThumbWheelRuntimeAction>,
    pub buttons: Vec<ButtonRuntimeBinding>,
    pub app_profiles: Vec<AppProfile>,
}

impl LocalRuntimePlan {
    pub fn requires_listener(&self) -> bool {
        self.thumb_wheel.is_some() || !self.buttons.is_empty() || !self.app_profiles.is_empty()
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();

        if let Some(thumb_wheel) = self.thumb_wheel {
            parts.push(format!("thumb wheel {}", thumb_wheel.label()));
        }
        if !self.buttons.is_empty() {
            parts.push(format!(
                "{} button binding{}",
                self.buttons.len(),
                plural(self.buttons.len())
            ));
        }
        if !self.app_profiles.is_empty() {
            parts.push(format!(
                "{} app profile{}",
                self.app_profiles.len(),
                plural(self.app_profiles.len())
            ));
        }

        if parts.is_empty() {
            "no local actions".to_owned()
        } else {
            parts.join(", ")
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThumbWheelRuntimeAction {
    HorizontalScroll { speed_percent: u16 },
    TabSwitch,
    Zoom,
    Volume,
}

impl ThumbWheelRuntimeAction {
    pub fn label(self) -> String {
        match self {
            Self::HorizontalScroll { speed_percent } => {
                format!("horizontal scroll at {speed_percent}%")
            }
            Self::TabSwitch => "tab switch".to_owned(),
            Self::Zoom => "zoom".to_owned(),
            Self::Volume => "volume".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ButtonRuntimeBinding {
    pub button: Master3sButton,
    pub action: ButtonAction,
    pub control_id: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Master3sRuntimeEvent {
    ThumbWheel {
        delta: i16,
        phase: Option<u8>,
        #[serde(default = "default_thumb_wheel_resolution")]
        resolution: u16,
        #[serde(default = "default_thumb_wheel_direction")]
        direction: i8,
    },
    DivertedButtons {
        buttons: Vec<Master3sButton>,
        unknown_control_ids: Vec<u16>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResolvedRuntimeAction {
    pub source: RuntimeActionSource,
    pub command: RuntimeCommand,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeActionSource {
    ThumbWheel,
    Button(Master3sButton),
    UnknownControlId(u16),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeCommand {
    KeyChord(Vec<RuntimeKey>),
    MouseButton(RuntimeMouseButton),
    HorizontalScroll {
        delta: i16,
        resolution: u16,
        direction: i8,
        speed_percent: u16,
    },
    Noop,
    Unsupported,
}

impl RuntimeCommand {
    pub fn label(&self) -> String {
        match self {
            Self::KeyChord(keys) => keys
                .iter()
                .map(|key| key.label())
                .collect::<Vec<_>>()
                .join("+"),
            Self::MouseButton(button) => button.label().to_owned(),
            Self::HorizontalScroll {
                delta,
                resolution,
                direction,
                speed_percent,
            } => format!(
                "horizontal scroll delta {delta}, resolution {resolution}, direction {direction}, speed {speed_percent}%"
            ),
            Self::Noop => "no action".to_owned(),
            Self::Unsupported => "unsupported".to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKey {
    Control,
    Shift,
    Alt,
    Super,
    Tab,
    Equal,
    Minus,
    Left,
    Right,
    C,
    V,
    VolumeUp,
    VolumeDown,
}

impl RuntimeKey {
    pub fn label(self) -> &'static str {
        match self {
            Self::Control => "Ctrl",
            Self::Shift => "Shift",
            Self::Alt => "Alt",
            Self::Super => "Super",
            Self::Tab => "Tab",
            Self::Equal => "=",
            Self::Minus => "-",
            Self::Left => "Left",
            Self::Right => "Right",
            Self::C => "C",
            Self::V => "V",
            Self::VolumeUp => "VolumeUp",
            Self::VolumeDown => "VolumeDown",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMouseButton {
    Middle,
}

impl RuntimeMouseButton {
    pub fn label(self) -> &'static str {
        match self {
            Self::Middle => "MouseMiddle",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SettingsApplyPlan {
    pub device_id: String,
    pub profile_name: String,
    pub steps: Vec<SettingsApplyStep>,
}

impl SettingsApplyPlan {
    pub fn requires_device_write(&self) -> bool {
        self.steps.iter().any(|step| step.requires_device_write)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SettingsApplyStep {
    pub operation: SettingsApplyOperation,
    pub feature: HidppFeature,
    pub requires_device_write: bool,
}

impl SettingsApplyStep {
    pub fn title(&self) -> String {
        self.operation.label()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SettingsApplyOperation {
    PointerSpeed {
        percent: u8,
    },
    WheelBehavior {
        mode: WheelRatchetMode,
        threshold: u8,
    },
    ScrollBehavior {
        high_resolution: Option<bool>,
        natural: Option<bool>,
    },
    ThumbWheel {
        mode: ThumbWheelMode,
        speed_percent: u16,
    },
    ButtonMapping {
        button: Master3sButton,
        action: ButtonAction,
    },
    LocalRuntime {
        thumb_wheel: Option<ThumbWheelRuntimeAction>,
        button_count: usize,
        app_profile_count: usize,
    },
    AppProfile {
        profile: AppProfile,
    },
}

impl SettingsApplyOperation {
    pub fn label(&self) -> String {
        match self {
            Self::PointerSpeed { percent } => format!("Set pointer speed to {percent}%"),
            Self::WheelBehavior { mode, threshold } => match mode {
                WheelRatchetMode::SmartShift => {
                    format!("Use SmartShift with sensitivity {threshold}")
                }
                _ => format!("Use {} scrolling", mode.label()),
            },
            Self::ScrollBehavior {
                high_resolution,
                natural,
            } => match (high_resolution, natural) {
                (Some(high_resolution), Some(natural)) => format!(
                    "{} smooth scrolling · {} direction",
                    if *high_resolution { "Use" } else { "Disable" },
                    if *natural { "natural" } else { "standard" }
                ),
                (Some(true), None) => "Enable smooth scrolling".to_owned(),
                (Some(false), None) => "Disable smooth scrolling".to_owned(),
                (None, Some(true)) => "Use the natural scroll direction".to_owned(),
                (None, Some(false)) => "Use the standard scroll direction".to_owned(),
                (None, None) => "Keep the current scroll behavior".to_owned(),
            },
            Self::ThumbWheel {
                mode,
                speed_percent,
            } => {
                if *mode == ThumbWheelMode::HorizontalScroll
                    && *speed_percent == DEFAULT_THUMB_WHEEL_SPEED_PERCENT
                {
                    "Use native horizontal scrolling".to_owned()
                } else if *mode == ThumbWheelMode::HorizontalScroll {
                    format!(
                        "Route thumb wheel through dogi for horizontal scrolling at {speed_percent}%"
                    )
                } else {
                    format!("Route thumb wheel through dogi for {}", mode.label())
                }
            }
            Self::ButtonMapping { button, action } => {
                if button_action_requires_runtime(*button, *action) {
                    format!(
                        "Route {} through dogi for {}",
                        button.label(),
                        action.label()
                    )
                } else {
                    format!("Use native behavior for {}", button.label())
                }
            }
            Self::LocalRuntime {
                thumb_wheel,
                button_count,
                app_profile_count,
            } => {
                let mut parts = Vec::new();
                if let Some(thumb_wheel) = thumb_wheel {
                    parts.push(format!("thumb wheel {}", thumb_wheel.label()));
                }
                if *button_count > 0 {
                    parts.push(format!(
                        "{button_count} button binding{}",
                        plural(*button_count)
                    ));
                }
                if *app_profile_count > 0 {
                    parts.push(format!(
                        "{app_profile_count} app profile{}",
                        plural(*app_profile_count)
                    ));
                }
                format!("Run local runtime listener for {}", parts.join(", "))
            }
            Self::AppProfile { profile } => format!(
                "Save local app profile {} at {}%, thumb wheel {}",
                profile.app_name,
                profile.pointer_speed_percent,
                profile.thumb_wheel.label()
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsApplyScope {
    Device,
    Local,
    Unsupported,
}

impl SettingsApplyScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Device => "device",
            Self::Local => "local",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SettingsApplyReport {
    pub device_id: String,
    pub profile_name: String,
    pub outcomes: Vec<SettingsApplyOutcome>,
}

impl SettingsApplyReport {
    pub fn has_failed_steps(&self) -> bool {
        self.outcomes
            .iter()
            .any(|outcome| outcome.status == SettingsApplyStatus::Failed)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SettingsApplyOutcome {
    pub title: String,
    pub feature: HidppFeature,
    pub status: SettingsApplyStatus,
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsApplyStatus {
    Applied,
    Skipped,
    Unsupported,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HidppFeature {
    PointerSpeed,
    SmartShift,
    HiresWheel,
    ThumbWheel,
    ReprogrammableControls,
    LocalRuntime,
    LocalProfile,
}

pub fn settings_apply_step_scope(
    step: &SettingsApplyStep,
    _settings: &Master3sSettings,
) -> SettingsApplyScope {
    if !step.requires_device_write {
        return SettingsApplyScope::Local;
    }

    SettingsApplyScope::Device
}

pub fn build_master3s_apply_plan(
    device_id: impl Into<String>,
    settings: &Master3sSettings,
) -> SettingsApplyPlan {
    let runtime_plan = build_master3s_runtime_plan(settings);
    let mut steps = vec![
        SettingsApplyStep {
            operation: SettingsApplyOperation::PointerSpeed {
                percent: settings.pointer_speed_percent,
            },
            feature: HidppFeature::PointerSpeed,
            requires_device_write: true,
        },
        SettingsApplyStep {
            operation: SettingsApplyOperation::WheelBehavior {
                mode: settings.ratchet_mode,
                threshold: settings.smart_shift_threshold,
            },
            feature: HidppFeature::SmartShift,
            requires_device_write: true,
        },
        SettingsApplyStep {
            operation: SettingsApplyOperation::ScrollBehavior {
                high_resolution: Some(settings.high_resolution_scroll),
                natural: Some(settings.natural_scroll),
            },
            feature: HidppFeature::HiresWheel,
            requires_device_write: true,
        },
        SettingsApplyStep {
            operation: SettingsApplyOperation::ThumbWheel {
                mode: settings.thumb_wheel,
                speed_percent: settings.thumb_wheel_speed_percent,
            },
            feature: HidppFeature::ThumbWheel,
            requires_device_write: true,
        },
    ];

    for binding in &settings.buttons {
        steps.push(SettingsApplyStep {
            operation: SettingsApplyOperation::ButtonMapping {
                button: binding.button,
                action: binding.action,
            },
            feature: HidppFeature::ReprogrammableControls,
            requires_device_write: true,
        });
    }

    if runtime_plan.requires_listener() {
        steps.push(SettingsApplyStep {
            operation: SettingsApplyOperation::LocalRuntime {
                thumb_wheel: runtime_plan.thumb_wheel,
                button_count: runtime_plan.buttons.len(),
                app_profile_count: runtime_plan.app_profiles.len(),
            },
            feature: HidppFeature::LocalRuntime,
            requires_device_write: false,
        });
    }

    for profile in &settings.app_profiles {
        steps.push(SettingsApplyStep {
            operation: SettingsApplyOperation::AppProfile {
                profile: profile.clone(),
            },
            feature: HidppFeature::LocalProfile,
            requires_device_write: false,
        });
    }

    SettingsApplyPlan {
        device_id: device_id.into(),
        profile_name: settings.profile_name.clone(),
        steps,
    }
}

pub fn build_master3s_device_diff_plan(
    device_id: impl Into<String>,
    baseline: &Master3sSettings,
    target: &Master3sSettings,
) -> SettingsApplyPlan {
    let baseline = baseline.normalized();
    let target = target.normalized();
    let mut plan = build_master3s_apply_plan(device_id, &target);

    plan.steps.retain_mut(|step| match &mut step.operation {
        SettingsApplyOperation::PointerSpeed { .. } => {
            baseline.pointer_speed_percent != target.pointer_speed_percent
        }
        SettingsApplyOperation::WheelBehavior { .. } => {
            baseline.smart_shift_enabled != target.smart_shift_enabled
                || baseline.smart_shift_threshold != target.smart_shift_threshold
                || baseline.ratchet_mode != target.ratchet_mode
        }
        SettingsApplyOperation::ScrollBehavior {
            high_resolution,
            natural,
        } => {
            *high_resolution = (baseline.high_resolution_scroll != target.high_resolution_scroll)
                .then_some(target.high_resolution_scroll);
            *natural =
                (baseline.natural_scroll != target.natural_scroll).then_some(target.natural_scroll);
            high_resolution.is_some() || natural.is_some()
        }
        SettingsApplyOperation::ThumbWheel { .. } => {
            baseline.thumb_wheel != target.thumb_wheel
                || baseline.thumb_wheel_speed_percent != target.thumb_wheel_speed_percent
        }
        SettingsApplyOperation::ButtonMapping { button, .. } => {
            baseline.button_action(*button) != target.button_action(*button)
        }
        SettingsApplyOperation::LocalRuntime { .. } | SettingsApplyOperation::AppProfile { .. } => {
            false
        }
    });

    plan
}

pub fn build_master3s_runtime_plan(settings: &Master3sSettings) -> LocalRuntimePlan {
    let settings = settings.normalized();
    let thumb_wheel =
        thumb_wheel_runtime_action(settings.thumb_wheel, settings.thumb_wheel_speed_percent);
    let buttons = settings
        .buttons
        .iter()
        .filter(|binding| button_action_requires_runtime(binding.button, binding.action))
        .map(|binding| ButtonRuntimeBinding {
            button: binding.button,
            action: binding.action,
            control_id: master3s_button_control_id(binding.button),
        })
        .collect();

    LocalRuntimePlan {
        profile_name: settings.profile_name,
        thumb_wheel,
        buttons,
        app_profiles: settings.app_profiles,
    }
}

pub fn effective_master3s_settings_for_app(
    settings: &Master3sSettings,
    active_application: Option<&ActiveApplication>,
) -> EffectiveAppSettings {
    let mut effective = settings.normalized();
    let matched_profile = active_application
        .and_then(|application| matching_app_profile(&effective, application))
        .cloned();

    if let Some(profile) = &matched_profile {
        effective.profile_name = format!("{} / {}", effective.profile_name, profile.app_name);
        effective.pointer_speed_percent = profile.pointer_speed_percent;
        effective.thumb_wheel = profile.thumb_wheel;
    }

    EffectiveAppSettings {
        settings: effective,
        matched_profile,
    }
}

pub fn matching_app_profile<'a>(
    settings: &'a Master3sSettings,
    active_application: &ActiveApplication,
) -> Option<&'a AppProfile> {
    settings
        .app_profiles
        .iter()
        .find(|profile| app_profile_matches(profile, active_application))
}

fn app_profile_matches(profile: &AppProfile, active_application: &ActiveApplication) -> bool {
    let needle = app_match_key(&profile.app_name);
    if needle.is_empty() {
        return false;
    }

    active_application
        .match_fields()
        .map(app_match_key)
        .any(|field| !field.is_empty() && field.contains(&needle))
}

pub fn thumb_wheel_runtime_action(
    mode: ThumbWheelMode,
    speed_percent: u16,
) -> Option<ThumbWheelRuntimeAction> {
    match mode {
        ThumbWheelMode::HorizontalScroll if speed_percent != DEFAULT_THUMB_WHEEL_SPEED_PERCENT => {
            Some(ThumbWheelRuntimeAction::HorizontalScroll { speed_percent })
        }
        ThumbWheelMode::TabSwitch => Some(ThumbWheelRuntimeAction::TabSwitch),
        ThumbWheelMode::Zoom => Some(ThumbWheelRuntimeAction::Zoom),
        ThumbWheelMode::Volume => Some(ThumbWheelRuntimeAction::Volume),
        ThumbWheelMode::HorizontalScroll | ThumbWheelMode::Disabled => None,
    }
}

pub fn master3s_button_control_id(button: Master3sButton) -> u16 {
    match button {
        Master3sButton::Middle => 0x0052,
        Master3sButton::Back => 0x0053,
        Master3sButton::Forward => 0x0056,
        Master3sButton::Gesture => 0x00c3,
        Master3sButton::ModeShift => 0x00c4,
    }
}

pub fn master3s_button_from_control_id(control_id: u16) -> Option<Master3sButton> {
    match control_id {
        0x0052 => Some(Master3sButton::Middle),
        0x0053 => Some(Master3sButton::Back),
        0x0056 => Some(Master3sButton::Forward),
        0x00c3 => Some(Master3sButton::Gesture),
        0x00c4 => Some(Master3sButton::ModeShift),
        _ => None,
    }
}

pub fn button_action_requires_runtime(button: Master3sButton, action: ButtonAction) -> bool {
    action != hardware_default_button_action(button)
}

pub fn resolve_master3s_runtime_event(
    plan: &LocalRuntimePlan,
    event: &Master3sRuntimeEvent,
) -> Vec<ResolvedRuntimeAction> {
    match event {
        Master3sRuntimeEvent::ThumbWheel {
            delta,
            resolution,
            direction,
            ..
        } => plan
            .thumb_wheel
            .and_then(|action| thumb_wheel_command(action, *delta, *resolution, *direction))
            .map(|command| ResolvedRuntimeAction {
                source: RuntimeActionSource::ThumbWheel,
                command,
            })
            .into_iter()
            .collect(),
        Master3sRuntimeEvent::DivertedButtons {
            buttons,
            unknown_control_ids,
        } => {
            let mut actions = buttons
                .iter()
                .filter_map(|button| {
                    plan.buttons
                        .iter()
                        .find(|binding| binding.button == *button)
                        .map(|binding| ResolvedRuntimeAction {
                            source: RuntimeActionSource::Button(*button),
                            command: button_action_command(binding.action),
                        })
                })
                .collect::<Vec<_>>();

            actions.extend(unknown_control_ids.iter().copied().map(|control_id| {
                ResolvedRuntimeAction {
                    source: RuntimeActionSource::UnknownControlId(control_id),
                    command: RuntimeCommand::Unsupported,
                }
            }));
            actions
        }
    }
}

fn thumb_wheel_command(
    action: ThumbWheelRuntimeAction,
    delta: i16,
    resolution: u16,
    direction: i8,
) -> Option<RuntimeCommand> {
    if delta == 0 {
        return None;
    }

    if let ThumbWheelRuntimeAction::HorizontalScroll { speed_percent } = action {
        return Some(RuntimeCommand::HorizontalScroll {
            delta,
            resolution: resolution.max(1),
            direction: normalized_thumb_wheel_direction(direction),
            speed_percent: speed_percent
                .clamp(MIN_THUMB_WHEEL_SPEED_PERCENT, MAX_THUMB_WHEEL_SPEED_PERCENT),
        });
    }

    let forward = delta > 0;
    let keys = match (action, forward) {
        (ThumbWheelRuntimeAction::TabSwitch, true) => vec![RuntimeKey::Control, RuntimeKey::Tab],
        (ThumbWheelRuntimeAction::TabSwitch, false) => {
            vec![RuntimeKey::Control, RuntimeKey::Shift, RuntimeKey::Tab]
        }
        (ThumbWheelRuntimeAction::Zoom, true) => vec![RuntimeKey::Control, RuntimeKey::Equal],
        (ThumbWheelRuntimeAction::Zoom, false) => vec![RuntimeKey::Control, RuntimeKey::Minus],
        (ThumbWheelRuntimeAction::Volume, true) => vec![RuntimeKey::VolumeUp],
        (ThumbWheelRuntimeAction::Volume, false) => vec![RuntimeKey::VolumeDown],
        (ThumbWheelRuntimeAction::HorizontalScroll { .. }, _) => unreachable!(),
    };
    Some(RuntimeCommand::KeyChord(keys))
}

fn button_action_command(action: ButtonAction) -> RuntimeCommand {
    match action {
        ButtonAction::Native => RuntimeCommand::Noop,
        ButtonAction::Back => RuntimeCommand::KeyChord(vec![RuntimeKey::Alt, RuntimeKey::Left]),
        ButtonAction::Forward => RuntimeCommand::KeyChord(vec![RuntimeKey::Alt, RuntimeKey::Right]),
        ButtonAction::Overview => RuntimeCommand::KeyChord(vec![RuntimeKey::Super]),
        ButtonAction::WindowSwitcher => {
            RuntimeCommand::KeyChord(vec![RuntimeKey::Alt, RuntimeKey::Tab])
        }
        ButtonAction::PreviousWorkspace => {
            RuntimeCommand::KeyChord(vec![RuntimeKey::Control, RuntimeKey::Alt, RuntimeKey::Left])
        }
        ButtonAction::NextWorkspace => RuntimeCommand::KeyChord(vec![
            RuntimeKey::Control,
            RuntimeKey::Alt,
            RuntimeKey::Right,
        ]),
        ButtonAction::MiddleClick => RuntimeCommand::MouseButton(RuntimeMouseButton::Middle),
        ButtonAction::Copy => RuntimeCommand::KeyChord(vec![RuntimeKey::Control, RuntimeKey::C]),
        ButtonAction::Paste => RuntimeCommand::KeyChord(vec![RuntimeKey::Control, RuntimeKey::V]),
        ButtonAction::Disabled => RuntimeCommand::Noop,
    }
}

fn default_master3s_buttons() -> Vec<ButtonBinding> {
    Master3sButton::ALL
        .into_iter()
        .map(|button| ButtonBinding {
            button,
            action: default_button_action(button),
        })
        .collect()
}

fn normalize_button_bindings(bindings: &[ButtonBinding]) -> Vec<ButtonBinding> {
    Master3sButton::ALL
        .into_iter()
        .map(|button| {
            let action = bindings
                .iter()
                .find(|binding| binding.button == button)
                .map(|binding| binding.action)
                .unwrap_or_else(|| default_button_action(button));
            ButtonBinding { button, action }
        })
        .collect()
}

fn default_button_action(button: Master3sButton) -> ButtonAction {
    let _ = button;
    ButtonAction::Native
}

fn hardware_default_button_action(button: Master3sButton) -> ButtonAction {
    let _ = button;
    ButtonAction::Native
}

fn clamp_u8(value: u8, min: u8, max: u8) -> u8 {
    value.clamp(min, max)
}

const fn default_thumb_wheel_speed_percent() -> u16 {
    DEFAULT_THUMB_WHEEL_SPEED_PERCENT
}

const fn default_thumb_wheel_resolution() -> u16 {
    1
}

const fn default_thumb_wheel_direction() -> i8 {
    1
}

const fn normalized_thumb_wheel_direction(direction: i8) -> i8 {
    if direction < 0 { -1 } else { 1 }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn non_empty_or_default(value: &str, default: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        default.to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn clean_optional_app_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn app_match_key(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_master3s_plan_is_device_only() {
        let settings = Master3sSettings::default();
        let plan = build_master3s_apply_plan("device-1", &settings);

        assert!(plan.requires_device_write());
        assert!(
            plan.steps
                .iter()
                .any(|step| step.feature == HidppFeature::PointerSpeed)
        );
        assert!(!plan.steps.iter().any(|step| !step.requires_device_write));
    }

    #[test]
    fn device_diff_plan_contains_only_changed_setting_groups() {
        let baseline = Master3sSettings::default();
        let mut target = baseline.clone();
        target.pointer_speed_percent = 125;
        target.natural_scroll = true;
        target.set_button_action(Master3sButton::Gesture, ButtonAction::Overview);

        let plan = build_master3s_device_diff_plan("device-1", &baseline, &target);

        assert_eq!(plan.steps.len(), 3);
        assert!(
            plan.steps
                .iter()
                .any(|step| step.feature == HidppFeature::PointerSpeed)
        );
        assert!(plan.steps.iter().any(|step| matches!(
            step.operation,
            SettingsApplyOperation::ScrollBehavior {
                high_resolution: None,
                natural: Some(true),
            }
        )));
        assert!(plan.steps.iter().any(|step| {
            matches!(
                step.operation,
                SettingsApplyOperation::ButtonMapping {
                    button: Master3sButton::Gesture,
                    ..
                }
            )
        }));
    }

    #[test]
    fn device_diff_plan_ignores_unchanged_and_local_only_settings() {
        let baseline = Master3sSettings::default();
        let mut target = baseline.clone();
        target.app_profiles.push(AppProfile {
            app_name: "Firefox".to_owned(),
            pointer_speed_percent: 90,
            thumb_wheel: ThumbWheelMode::TabSwitch,
        });

        assert!(
            build_master3s_device_diff_plan("device-1", &baseline, &baseline)
                .steps
                .is_empty()
        );
        assert!(
            build_master3s_device_diff_plan("device-1", &baseline, &target)
                .steps
                .is_empty()
        );
    }

    #[test]
    fn device_diff_plan_collapses_related_values_into_one_write() {
        let baseline = Master3sSettings::default();
        let target = Master3sSettings {
            high_resolution_scroll: false,
            natural_scroll: true,
            ..baseline.clone()
        };

        let plan = build_master3s_device_diff_plan("device-1", &baseline, &target);

        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].feature, HidppFeature::HiresWheel);
        assert!(matches!(
            plan.steps[0].operation,
            SettingsApplyOperation::ScrollBehavior {
                high_resolution: Some(false),
                natural: Some(true),
            }
        ));
    }

    #[test]
    fn device_diff_plan_preserves_unchanged_hires_wheel_flags() {
        let baseline = Master3sSettings::default();
        let target = Master3sSettings {
            high_resolution_scroll: false,
            ..baseline.clone()
        };

        let plan = build_master3s_device_diff_plan("device-1", &baseline, &target);

        assert_eq!(plan.steps.len(), 1);
        assert!(matches!(
            plan.steps[0].operation,
            SettingsApplyOperation::ScrollBehavior {
                high_resolution: Some(false),
                natural: None,
            }
        ));
    }

    #[test]
    fn scopes_device_local_and_unsupported_master3s_steps() {
        let thumb_step = SettingsApplyStep {
            operation: SettingsApplyOperation::ThumbWheel {
                mode: ThumbWheelMode::HorizontalScroll,
                speed_percent: DEFAULT_THUMB_WHEEL_SPEED_PERCENT,
            },
            feature: HidppFeature::ThumbWheel,
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
        let horizontal = Master3sSettings {
            thumb_wheel: ThumbWheelMode::HorizontalScroll,
            ..Master3sSettings::default()
        };
        let zoom = Master3sSettings {
            thumb_wheel: ThumbWheelMode::Zoom,
            ..Master3sSettings::default()
        };

        assert_eq!(
            settings_apply_step_scope(&thumb_step, &horizontal),
            SettingsApplyScope::Device
        );
        assert_eq!(
            settings_apply_step_scope(&thumb_step, &zoom),
            SettingsApplyScope::Device
        );
        assert_eq!(
            settings_apply_step_scope(&button_step, &horizontal),
            SettingsApplyScope::Device
        );
        assert_eq!(
            settings_apply_step_scope(&local_step, &horizontal),
            SettingsApplyScope::Local
        );
    }

    #[test]
    fn builds_runtime_plan_for_software_actions() {
        let settings = Master3sSettings {
            thumb_wheel: ThumbWheelMode::Zoom,
            buttons: vec![
                ButtonBinding {
                    button: Master3sButton::Back,
                    action: ButtonAction::Native,
                },
                ButtonBinding {
                    button: Master3sButton::Gesture,
                    action: ButtonAction::Copy,
                },
            ],
            app_profiles: vec![AppProfile {
                app_name: "Browser".to_owned(),
                pointer_speed_percent: 100,
                thumb_wheel: ThumbWheelMode::TabSwitch,
            }],
            ..Master3sSettings::default()
        };

        let plan = build_master3s_runtime_plan(&settings);

        assert_eq!(plan.thumb_wheel, Some(ThumbWheelRuntimeAction::Zoom));
        assert!(plan.requires_listener());
        assert!(plan.summary().contains("thumb wheel zoom"));
        assert!(
            plan.buttons
                .iter()
                .any(|binding| binding.button == Master3sButton::Gesture
                    && binding.action == ButtonAction::Copy
                    && binding.control_id == 0x00c3)
        );
        assert!(
            !plan
                .buttons
                .iter()
                .any(|binding| binding.button == Master3sButton::Back)
        );
    }

    #[test]
    fn custom_horizontal_scroll_speed_uses_local_runtime() {
        let settings = Master3sSettings {
            thumb_wheel: ThumbWheelMode::HorizontalScroll,
            thumb_wheel_speed_percent: 150,
            ..Master3sSettings::default()
        };
        let plan = build_master3s_runtime_plan(&settings);

        assert_eq!(
            plan.thumb_wheel,
            Some(ThumbWheelRuntimeAction::HorizontalScroll { speed_percent: 150 })
        );
        assert!(plan.requires_listener());
        assert_eq!(
            resolve_master3s_runtime_event(
                &plan,
                &Master3sRuntimeEvent::ThumbWheel {
                    delta: -15,
                    phase: Some(2),
                    resolution: 8,
                    direction: -1,
                },
            ),
            vec![ResolvedRuntimeAction {
                source: RuntimeActionSource::ThumbWheel,
                command: RuntimeCommand::HorizontalScroll {
                    delta: -15,
                    resolution: 8,
                    direction: -1,
                    speed_percent: 150,
                },
            }]
        );
    }

    #[test]
    fn native_horizontal_scroll_at_default_speed_needs_no_listener() {
        let plan = build_master3s_runtime_plan(&Master3sSettings::default());

        assert_eq!(plan.thumb_wheel, None);
        assert!(!plan.requires_listener());
    }

    #[test]
    fn maps_master3s_button_control_ids() {
        for button in Master3sButton::ALL {
            let control_id = master3s_button_control_id(button);

            assert_eq!(master3s_button_from_control_id(control_id), Some(button));
        }

        assert_eq!(master3s_button_from_control_id(0xbeef), None);
    }

    #[test]
    fn matches_active_application_to_app_profile() {
        let settings = Master3sSettings {
            app_profiles: vec![AppProfile {
                app_name: "Firefox".to_owned(),
                pointer_speed_percent: 80,
                thumb_wheel: ThumbWheelMode::TabSwitch,
            }],
            ..Master3sSettings::default()
        };
        let active = ActiveApplication::new(
            Some("Mozilla Firefox".to_owned()),
            Some("firefox".to_owned()),
            Some("/usr/bin/firefox".to_owned()),
        );

        let effective = effective_master3s_settings_for_app(&settings, Some(&active));

        assert_eq!(
            effective
                .matched_profile
                .as_ref()
                .map(|profile| profile.app_name.as_str()),
            Some("Firefox")
        );
        assert_eq!(effective.settings.pointer_speed_percent, 80);
        assert_eq!(effective.settings.thumb_wheel, ThumbWheelMode::TabSwitch);
    }

    #[test]
    fn app_profile_matching_preserves_base_settings_without_match() {
        let settings = Master3sSettings {
            pointer_speed_percent: 125,
            thumb_wheel: ThumbWheelMode::Zoom,
            app_profiles: vec![AppProfile {
                app_name: "Firefox".to_owned(),
                pointer_speed_percent: 80,
                thumb_wheel: ThumbWheelMode::TabSwitch,
            }],
            ..Master3sSettings::default()
        };
        let active = ActiveApplication::new(Some("Terminal".to_owned()), None, None);

        let effective = effective_master3s_settings_for_app(&settings, Some(&active));

        assert_eq!(effective.matched_profile, None);
        assert_eq!(effective.settings.pointer_speed_percent, 125);
        assert_eq!(effective.settings.thumb_wheel, ThumbWheelMode::Zoom);
    }

    #[test]
    fn resolves_thumb_wheel_runtime_events_to_key_chords() {
        let settings = Master3sSettings {
            thumb_wheel: ThumbWheelMode::Zoom,
            ..Master3sSettings::default()
        };
        let plan = build_master3s_runtime_plan(&settings);

        assert_eq!(
            resolve_master3s_runtime_event(
                &plan,
                &Master3sRuntimeEvent::ThumbWheel {
                    delta: 12,
                    phase: None,
                    resolution: 1,
                    direction: 1,
                },
            ),
            vec![ResolvedRuntimeAction {
                source: RuntimeActionSource::ThumbWheel,
                command: RuntimeCommand::KeyChord(vec![RuntimeKey::Control, RuntimeKey::Equal]),
            }]
        );
        assert_eq!(
            resolve_master3s_runtime_event(
                &plan,
                &Master3sRuntimeEvent::ThumbWheel {
                    delta: -12,
                    phase: None,
                    resolution: 1,
                    direction: 1,
                },
            ),
            vec![ResolvedRuntimeAction {
                source: RuntimeActionSource::ThumbWheel,
                command: RuntimeCommand::KeyChord(vec![RuntimeKey::Control, RuntimeKey::Minus]),
            }]
        );
        assert!(
            resolve_master3s_runtime_event(
                &plan,
                &Master3sRuntimeEvent::ThumbWheel {
                    delta: 0,
                    phase: None,
                    resolution: 1,
                    direction: 1,
                },
            )
            .is_empty()
        );
    }

    #[test]
    fn resolves_diverted_button_runtime_events() {
        let settings = Master3sSettings {
            buttons: vec![ButtonBinding {
                button: Master3sButton::Gesture,
                action: ButtonAction::Copy,
            }],
            ..Master3sSettings::default()
        };
        let plan = build_master3s_runtime_plan(&settings);

        assert_eq!(
            resolve_master3s_runtime_event(
                &plan,
                &Master3sRuntimeEvent::DivertedButtons {
                    buttons: vec![Master3sButton::Gesture],
                    unknown_control_ids: vec![0xbeef],
                },
            ),
            vec![
                ResolvedRuntimeAction {
                    source: RuntimeActionSource::Button(Master3sButton::Gesture),
                    command: RuntimeCommand::KeyChord(vec![RuntimeKey::Control, RuntimeKey::C]),
                },
                ResolvedRuntimeAction {
                    source: RuntimeActionSource::UnknownControlId(0xbeef),
                    command: RuntimeCommand::Unsupported,
                },
            ]
        );
    }

    #[test]
    fn normalizes_user_facing_ranges() {
        let settings = Master3sSettings {
            profile_name: "  ".to_owned(),
            pointer_speed_percent: 250,
            smart_shift_threshold: 0,
            thumb_wheel_speed_percent: 900,
            app_profiles: vec![AppProfile {
                app_name: "  ".to_owned(),
                pointer_speed_percent: 10,
                thumb_wheel: ThumbWheelMode::Zoom,
            }],
            ..Master3sSettings::default()
        };

        let normalized = settings.normalized();

        assert_eq!(normalized.profile_name, "Default");
        assert_eq!(normalized.pointer_speed_percent, 200);
        assert_eq!(normalized.smart_shift_threshold, 1);
        assert_eq!(
            normalized.thumb_wheel_speed_percent,
            MAX_THUMB_WHEEL_SPEED_PERCENT
        );
        assert_eq!(normalized.app_profiles[0].app_name, "Application");
        assert_eq!(normalized.app_profiles[0].pointer_speed_percent, 50);
    }

    #[test]
    fn normalizes_button_bindings_to_master3s_order() {
        let settings = Master3sSettings {
            buttons: vec![
                ButtonBinding {
                    button: Master3sButton::Gesture,
                    action: ButtonAction::Copy,
                },
                ButtonBinding {
                    button: Master3sButton::Gesture,
                    action: ButtonAction::Paste,
                },
            ],
            ..Master3sSettings::default()
        };

        let normalized = settings.normalized();

        assert_eq!(normalized.buttons.len(), Master3sButton::ALL.len());
        assert_eq!(
            normalized.button_action(Master3sButton::Back),
            ButtonAction::Native
        );
        assert_eq!(
            normalized.button_action(Master3sButton::Gesture),
            ButtonAction::Copy
        );
    }

    #[test]
    fn can_update_button_action() {
        let mut settings = Master3sSettings::default();

        settings.set_button_action(Master3sButton::Back, ButtonAction::Paste);

        assert_eq!(
            settings.button_action(Master3sButton::Back),
            ButtonAction::Paste
        );
    }
}
