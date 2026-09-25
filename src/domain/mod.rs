mod device;
mod error;
mod settings;

pub use device::{
    BatteryInfo, BatterySource, BatteryStatus, BusKind, CapabilityState, ConnectionKind,
    DeviceAccess, DeviceCapabilities, DeviceInfo, HidUsage, HidppFeatureInfo, HidppProbeIssue,
    HidppProtocolVersion, LOGITECH_VENDOR_ID, PairedDeviceInfo, ReceiverKind, ReportDescriptorInfo,
    WritePolicy, bus_kind_from_linux_bus_id, device_settings_id, hidpp_endpoint_id,
    infer_connection, infer_receiver_kind, is_logitech_vendor, known_logitech_model_name,
    known_logitech_product_name, known_logitech_wpid_name, logical_hidpp_device_id,
    resolved_logitech_device_name, stable_device_id,
};
pub use error::{DogiError, Result};
pub use settings::{
    Action, ActiveApplication, AppProfile, AppProfileOverrides, ApplicationMatchField,
    ApplicationMatcher, ButtonAction, ButtonBinding, DEFAULT_THUMB_WHEEL_SPEED_PERCENT,
    DeviceSettingValue, GestureBindings, GestureDirection, HidppFeature, LocalRuntimePlan,
    MAX_THUMB_WHEEL_SPEED_PERCENT, MIN_THUMB_WHEEL_SPEED_PERCENT, Master3sButton,
    Master3sRuntimeEvent, Master3sSettings, ResolvedRuntimeAction, RuntimeActionResolver,
    RuntimeActionSource, RuntimeCommand, RuntimeKey, RuntimeMouseButton, SettingsApplyOperation,
    SettingsApplyOutcome, SettingsApplyPlan, SettingsApplyPreview, SettingsApplyPreviewStep,
    SettingsApplyReport, SettingsApplyScope, SettingsApplyStatus, SettingsApplyStep,
    SettingsTransactionState, ThumbWheelMode, ThumbWheelRuntimeAction, WheelRatchetMode,
    build_master3s_apply_plan, build_master3s_device_diff_plan, build_master3s_runtime_plan,
    button_action_requires_runtime, effective_master3s_settings_for_app,
    master3s_button_control_id, master3s_button_from_control_id, settings_apply_step_scope,
};
