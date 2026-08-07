mod device;
mod error;
mod settings;

pub use device::{
    BatteryInfo, BatterySource, BatteryStatus, BusKind, CapabilityState, ConnectionKind,
    DeviceAccess, DeviceCapabilities, DeviceConfig, DeviceInfo, HidUsage, HidppFeatureInfo,
    HidppProtocolVersion, LOGITECH_VENDOR_ID, PairedDeviceInfo, ReceiverKind, ReportDescriptorInfo,
    WritePolicy, bus_kind_from_linux_bus_id, device_settings_id, infer_connection,
    infer_receiver_kind, is_hidpp_usage, is_logitech_vendor, known_logitech_model_name,
    known_logitech_product_name, known_logitech_wpid_name, resolved_logitech_device_name,
    resolved_logitech_paired_device_name, stable_device_id,
};
pub use error::{DogiError, Result};
pub use settings::{
    Action, ActiveApplication, AppProfile, AppProfileOverrides, ApplicationMatchField,
    ApplicationMatcher, ButtonAction, ButtonBinding, ButtonRuntimeBinding,
    DEFAULT_GESTURE_THRESHOLD, DEFAULT_THUMB_WHEEL_SPEED_PERCENT, EffectiveAppSettings,
    GestureBindings, GestureDirection, HidppFeature, LocalRuntimePlan, MAX_GESTURE_THRESHOLD,
    MAX_THUMB_WHEEL_SPEED_PERCENT, MIN_GESTURE_THRESHOLD, MIN_THUMB_WHEEL_SPEED_PERCENT,
    Master3sButton, Master3sRuntimeEvent, Master3sSettings, ResolvedRuntimeAction,
    RuntimeActionResolver, RuntimeActionSource, RuntimeCommand, RuntimeKey, RuntimeMouseButton,
    SettingsApplyOperation, SettingsApplyOutcome, SettingsApplyPlan, SettingsApplyReport,
    SettingsApplyScope, SettingsApplyStatus, SettingsApplyStep, ThumbWheelMode,
    ThumbWheelRuntimeAction, WheelRatchetMode, build_master3s_apply_plan,
    build_master3s_device_diff_plan, build_master3s_runtime_plan, button_action_requires_runtime,
    effective_master3s_settings_for_app, master3s_button_control_id,
    master3s_button_from_control_id, matching_app_profile, resolve_master3s_runtime_event,
    settings_apply_step_scope, thumb_wheel_runtime_action,
};

pub const APP_NAME: &str = "dogi";

pub fn app_description() -> &'static str {
    "dogi: lightweight Logitech mouse configuration"
}
