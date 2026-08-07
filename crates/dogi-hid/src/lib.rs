use std::time::Duration;

use dogi_core::{
    DeviceInfo, DogiError, HidppFeatureInfo, Master3sRuntimeEvent, Master3sSettings, Result,
    SettingsApplyPlan, SettingsApplyReport, build_master3s_apply_plan,
    master3s_button_from_control_id,
};

const HIDPP_SHORT_REPORT_ID: u8 = 0x10;
const HIDPP_LONG_REPORT_ID: u8 = 0x11;
const HIDPP_FEATURE_REPROG_CONTROLS_V4: u16 = 0x1b04;
const HIDPP_FEATURE_THUMB_WHEEL: u16 = 0x2150;

pub fn parse_master3s_runtime_notification(
    report: &[u8],
    features: &[HidppFeatureInfo],
) -> Option<Master3sRuntimeEvent> {
    let report_id = *report.first()?;
    if report_id != HIDPP_SHORT_REPORT_ID && report_id != HIDPP_LONG_REPORT_ID {
        return None;
    }

    let feature_index = *report.get(2)?;
    let address = *report.get(3)?;
    if feature_index == 0 || address & 0x0f != 0 {
        return None;
    }

    let function = address >> 4;
    let data = report.get(4..)?;
    let feature_id = features
        .iter()
        .find(|feature| feature.index == feature_index)
        .map(|feature| feature.feature_id)?;

    match (feature_id, function) {
        (HIDPP_FEATURE_THUMB_WHEEL, 0) => parse_thumb_wheel_runtime_event(data),
        (HIDPP_FEATURE_REPROG_CONTROLS_V4, 0) => parse_diverted_buttons_runtime_event(data),
        (HIDPP_FEATURE_REPROG_CONTROLS_V4, 1) => parse_raw_movement_runtime_event(data),
        _ => None,
    }
}

fn parse_thumb_wheel_runtime_event(data: &[u8]) -> Option<Master3sRuntimeEvent> {
    let delta = i16::from_be_bytes([*data.first()?, *data.get(1)?]);
    Some(Master3sRuntimeEvent::ThumbWheel {
        delta,
        phase: data.get(4).copied(),
        resolution: 1,
        direction: 1,
    })
}

fn parse_diverted_buttons_runtime_event(data: &[u8]) -> Option<Master3sRuntimeEvent> {
    let control_bytes = data.get(..8)?;
    let mut buttons = Vec::new();
    let mut unknown_control_ids = Vec::new();

    for chunk in control_bytes.chunks_exact(2) {
        let control_id = u16::from_be_bytes([chunk[0], chunk[1]]);
        if control_id == 0 {
            continue;
        }

        if let Some(button) = master3s_button_from_control_id(control_id) {
            buttons.push(button);
        } else {
            unknown_control_ids.push(control_id);
        }
    }

    Some(Master3sRuntimeEvent::DivertedButtons {
        buttons,
        unknown_control_ids,
    })
}

fn parse_raw_movement_runtime_event(data: &[u8]) -> Option<Master3sRuntimeEvent> {
    Some(Master3sRuntimeEvent::RawMovement {
        x: i16::from_be_bytes([*data.first()?, *data.get(1)?]),
        y: i16::from_be_bytes([*data.get(2)?, *data.get(3)?]),
    })
}

#[cfg(test)]
mod runtime_notification_tests {
    use super::*;
    use dogi_core::Master3sButton;

    #[test]
    fn parses_thumb_wheel_runtime_notification() {
        let features = vec![feature(16, HIDPP_FEATURE_THUMB_WHEEL)];
        let report = [HIDPP_SHORT_REPORT_ID, 1, 16, 0x00, 0xff, 0xec, 0, 0, 1];

        assert_eq!(
            parse_master3s_runtime_notification(&report, &features),
            Some(Master3sRuntimeEvent::ThumbWheel {
                delta: -20,
                phase: Some(1),
                resolution: 1,
                direction: 1,
            })
        );
    }

    #[test]
    fn parses_diverted_button_runtime_notification() {
        let features = vec![feature(9, HIDPP_FEATURE_REPROG_CONTROLS_V4)];
        let report = [
            HIDPP_LONG_REPORT_ID,
            1,
            9,
            0x00,
            0x00,
            0x53,
            0x00,
            0xc3,
            0xbe,
            0xef,
            0,
            0,
        ];

        assert_eq!(
            parse_master3s_runtime_notification(&report, &features),
            Some(Master3sRuntimeEvent::DivertedButtons {
                buttons: vec![Master3sButton::Back, Master3sButton::Gesture],
                unknown_control_ids: vec![0xbeef],
            })
        );
    }

    #[test]
    fn parses_raw_movement_notification() {
        let features = vec![feature(9, HIDPP_FEATURE_REPROG_CONTROLS_V4)];
        let report = [HIDPP_LONG_REPORT_ID, 1, 9, 0x10, 0xff, 0xec, 0x00, 0x19];

        assert_eq!(
            parse_master3s_runtime_notification(&report, &features),
            Some(Master3sRuntimeEvent::RawMovement { x: -20, y: 25 })
        );
    }

    #[test]
    fn ignores_request_replies_and_unknown_features() {
        let features = vec![feature(16, HIDPP_FEATURE_THUMB_WHEEL)];
        let reply = [HIDPP_SHORT_REPORT_ID, 1, 16, 0x08, 0, 0];
        let unknown_feature = [HIDPP_SHORT_REPORT_ID, 1, 17, 0x00, 0, 0];

        assert_eq!(parse_master3s_runtime_notification(&reply, &features), None);
        assert_eq!(
            parse_master3s_runtime_notification(&unknown_feature, &features),
            None
        );
    }

    #[test]
    fn partial_apply_rejects_a_plan_for_another_device_before_io() {
        let settings = Master3sSettings::default();
        let plan = build_master3s_apply_plan("device-a", &settings);

        let error = apply_master3s_settings_plan("device-b", &settings, &plan)
            .expect_err("mismatched plan must be rejected");

        assert!(matches!(error, DogiError::InvalidArgument(_)));
    }

    fn feature(index: u8, feature_id: u16) -> HidppFeatureInfo {
        HidppFeatureInfo {
            index,
            feature_id,
            name: format!("FEATURE_{feature_id:04X}"),
            flags: 0,
            version: 1,
        }
    }
}

pub trait HidBackend {
    fn scan(&self) -> Result<Vec<DeviceInfo>>;

    fn find(&self, id: &str) -> Result<DeviceInfo> {
        self.scan()?
            .into_iter()
            .find(|device| device.id == id)
            .ok_or(DogiError::DeviceNotFound)
    }
}

pub fn scan_devices() -> Result<Vec<DeviceInfo>> {
    platform::scan_devices()
}

pub fn scan_device_inventory() -> Result<Vec<DeviceInfo>> {
    platform::scan_device_inventory()
}

pub fn scan_devices_for_ui() -> Result<Vec<DeviceInfo>> {
    platform::scan_devices_for_ui()
}

pub fn scan_all_devices() -> Result<Vec<DeviceInfo>> {
    platform::scan_all_devices()
}

pub fn find_device(id: &str) -> Result<DeviceInfo> {
    platform::find_device(id)
}

pub fn apply_master3s_settings(
    device_id: &str,
    settings: &Master3sSettings,
) -> Result<SettingsApplyReport> {
    let settings = settings.normalized();
    let plan = build_master3s_apply_plan(device_id, &settings);
    platform::apply_master3s_settings_plan(device_id, &settings, &plan)
}

pub fn apply_master3s_settings_plan(
    device_id: &str,
    settings: &Master3sSettings,
    plan: &SettingsApplyPlan,
) -> Result<SettingsApplyReport> {
    if plan.device_id != device_id {
        return Err(DogiError::InvalidArgument(format!(
            "apply plan targets {}, not {device_id}",
            plan.device_id
        )));
    }
    platform::apply_master3s_settings_plan(device_id, &settings.normalized(), plan)
}

pub fn listen_master3s_runtime_events(
    device_id: &str,
    event_limit: usize,
    idle_timeout: Duration,
) -> Result<Vec<Master3sRuntimeEvent>> {
    platform::listen_master3s_runtime_events(device_id, event_limit, idle_timeout)
}

pub use platform::Master3sRuntimeEventListener;

#[cfg(target_os = "linux")]
mod platform {
    use std::collections::HashMap;
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::os::unix::io::AsRawFd;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use dogi_core::{
        BatteryInfo, BatterySource, BatteryStatus, ButtonBinding, CapabilityState, ConnectionKind,
        DeviceAccess, DeviceCapabilities, DeviceInfo, DogiError, HidUsage, HidppFeature,
        HidppFeatureInfo, HidppProtocolVersion, Master3sSettings, PairedDeviceInfo, ReceiverKind,
        ReportDescriptorInfo, Result, SettingsApplyOperation, SettingsApplyOutcome,
        SettingsApplyPlan, SettingsApplyReport, SettingsApplyStatus, ThumbWheelMode,
        WheelRatchetMode, WritePolicy, bus_kind_from_linux_bus_id, button_action_requires_runtime,
        infer_connection, infer_receiver_kind, is_logitech_vendor, master3s_button_control_id,
        stable_device_id,
    };

    use crate::{HidBackend, Master3sRuntimeEvent, parse_master3s_runtime_notification};

    const HIDRAW_CLASS: &str = "/sys/class/hidraw";
    const HIDPP_USAGE_PAGE: u16 = 0xff00;
    const HIDPP_SHORT_REPORT_ID: u8 = 0x10;
    const HIDPP_LONG_REPORT_ID: u8 = 0x11;
    const HIDPP_SHORT_REPORT_LEN: usize = 7;
    const HIDPP_LONG_REPORT_LEN: usize = 20;
    const HIDPP_MAX_READ_LEN: usize = 32;
    const HIDPP_SW_ID: u8 = 0x08;
    const HIDPP_FEATURE_ROOT: u16 = 0x0000;
    const HIDPP_FEATURE_FEATURE_SET: u16 = 0x0001;
    const HIDPP_FEATURE_DEVICE_FW_VERSION: u16 = 0x0003;
    const HIDPP_FEATURE_DEVICE_NAME: u16 = 0x0005;
    const HIDPP_FEATURE_BATTERY_STATUS: u16 = 0x1000;
    const HIDPP_FEATURE_BATTERY_VOLTAGE: u16 = 0x1001;
    const HIDPP_FEATURE_UNIFIED_BATTERY: u16 = 0x1004;
    const HIDPP_FEATURE_REPROG_CONTROLS: u16 = 0x1b00;
    const HIDPP_FEATURE_REPROG_CONTROLS_V2: u16 = 0x1b01;
    const HIDPP_FEATURE_REPROG_CONTROLS_V3: u16 = 0x1b03;
    const HIDPP_FEATURE_REPROG_CONTROLS_V4: u16 = 0x1b04;
    const HIDPP_FEATURE_SMART_SHIFT: u16 = 0x2110;
    const HIDPP_FEATURE_SMART_SHIFT_ENHANCED: u16 = 0x2111;
    const HIDPP_FEATURE_HIRES_WHEEL: u16 = 0x2121;
    const HIDPP_FEATURE_THUMB_WHEEL: u16 = 0x2150;
    const HIDPP_FEATURE_MOUSE_POINTER: u16 = 0x2200;
    const HIDPP_FEATURE_ADJUSTABLE_DPI: u16 = 0x2201;
    const HIDPP_FEATURE_EXTENDED_ADJUSTABLE_DPI: u16 = 0x2202;
    const HIDPP_FEATURE_POINTER_SPEED: u16 = 0x2205;
    const HIDPP_FEATURE_ONBOARD_PROFILES: u16 = 0x8100;
    const REPROG_KEY_FLAG_DIVERTABLE: u16 = 0x0020;
    const REPROG_KEY_FLAG_VIRTUAL: u16 = 0x0080;
    const REPROG_KEY_FLAG_RAW_XY: u16 = 0x0100;
    const REPROG_MAPPING_FLAG_DIVERTED: u8 = 0x01;
    const REPROG_MAPPING_FLAG_DIVERTED_VALID: u8 = 0x02;
    const REPROG_MAPPING_FLAG_RAW_XY: u8 = 0x10;
    const REPROG_MAPPING_FLAG_RAW_XY_VALID: u8 = 0x20;
    const HIDPP_FEATURE_PROFILE_MANAGEMENT: u16 = 0x8101;
    const HIDPP_DOGI_FEATURE_IDS: &[u16] = &[
        HIDPP_FEATURE_DEVICE_FW_VERSION,
        HIDPP_FEATURE_DEVICE_NAME,
        HIDPP_FEATURE_BATTERY_STATUS,
        HIDPP_FEATURE_BATTERY_VOLTAGE,
        HIDPP_FEATURE_UNIFIED_BATTERY,
        HIDPP_FEATURE_REPROG_CONTROLS,
        HIDPP_FEATURE_REPROG_CONTROLS_V2,
        HIDPP_FEATURE_REPROG_CONTROLS_V3,
        HIDPP_FEATURE_REPROG_CONTROLS_V4,
        HIDPP_FEATURE_SMART_SHIFT,
        HIDPP_FEATURE_SMART_SHIFT_ENHANCED,
        HIDPP_FEATURE_HIRES_WHEEL,
        HIDPP_FEATURE_THUMB_WHEEL,
        HIDPP_FEATURE_MOUSE_POINTER,
        HIDPP_FEATURE_ADJUSTABLE_DPI,
        HIDPP_FEATURE_EXTENDED_ADJUSTABLE_DPI,
        HIDPP_FEATURE_POINTER_SPEED,
        HIDPP_FEATURE_ONBOARD_PROFILES,
        HIDPP_FEATURE_PROFILE_MANAGEMENT,
    ];
    const HIDPP_RECEIVER_DEVNUMBER: u8 = 0xff;
    const HIDPP_REGISTER_RECEIVER_INFO: u16 = 0x02b5;
    const HIDPP_PAIRING_INFORMATION: u8 = 0x20;
    const HIDPP_EXTENDED_PAIRING_INFORMATION: u8 = 0x30;
    const HIDPP_DEVICE_NAME: u8 = 0x40;
    const HIDPP_BOLT_PAIRING_INFORMATION: u8 = 0x50;
    const HIDPP_BOLT_DEVICE_NAME: u8 = 0x60;
    const HIDPP_PING_TIMEOUT: Duration = Duration::from_millis(500);
    const HIDPP_REQUEST_TIMEOUT: Duration = Duration::from_millis(1_200);
    const HIDPP_PROBE_RETRY_DELAYS: [Duration; 2] =
        [Duration::from_millis(120), Duration::from_millis(360)];

    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    enum HidppScanDepth {
        Inventory,
        DogiFeatures,
        Full,
    }

    #[derive(Debug, Default)]
    pub struct SysfsHidBackend;

    impl HidBackend for SysfsHidBackend {
        fn scan(&self) -> Result<Vec<DeviceInfo>> {
            Ok(self
                .scan_all()?
                .into_iter()
                .filter(DeviceInfo::is_logitech)
                .collect())
        }
    }

    impl SysfsHidBackend {
        pub fn scan_all(&self) -> Result<Vec<DeviceInfo>> {
            self.scan_all_with_depth(HidppScanDepth::Full)
        }

        pub fn scan_inventory(&self) -> Result<Vec<DeviceInfo>> {
            self.scan_all_with_depth(HidppScanDepth::Inventory)
        }

        pub fn scan_for_ui(&self) -> Result<Vec<DeviceInfo>> {
            self.scan_all_with_depth(HidppScanDepth::DogiFeatures)
        }

        fn scan_all_with_depth(&self, depth: HidppScanDepth) -> Result<Vec<DeviceInfo>> {
            let entries = match fs::read_dir(HIDRAW_CLASS) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Vec::new());
                }
                Err(error) => return Err(DogiError::BackendUnavailable(error.to_string())),
            };
            let mut devices = Vec::new();

            for entry in entries {
                let entry = entry.map_err(|error| DogiError::Transport(error.to_string()))?;
                let Some(hidraw_name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                    continue;
                };

                if !hidraw_name.starts_with("hidraw") {
                    continue;
                }

                if let Some(device) = read_hidraw_device(&entry.path(), &hidraw_name, depth)? {
                    devices.push(device);
                }
            }

            devices.sort_by(|left, right| {
                left.name
                    .cmp(&right.name)
                    .then_with(|| left.product_key().cmp(&right.product_key()))
                    .then_with(|| left.interface_number.cmp(&right.interface_number))
                    .then_with(|| left.path.cmp(&right.path))
            });

            Ok(devices)
        }
    }

    pub fn scan_devices() -> Result<Vec<DeviceInfo>> {
        SysfsHidBackend.scan()
    }

    pub fn scan_device_inventory() -> Result<Vec<DeviceInfo>> {
        Ok(SysfsHidBackend
            .scan_inventory()?
            .into_iter()
            .filter(DeviceInfo::is_logitech)
            .collect())
    }

    pub fn scan_devices_for_ui() -> Result<Vec<DeviceInfo>> {
        Ok(SysfsHidBackend
            .scan_for_ui()?
            .into_iter()
            .filter(DeviceInfo::is_logitech)
            .collect())
    }

    pub fn scan_all_devices() -> Result<Vec<DeviceInfo>> {
        SysfsHidBackend.scan_all()
    }

    pub fn find_device(id: &str) -> Result<DeviceInfo> {
        SysfsHidBackend.find(id)
    }

    pub fn apply_master3s_settings_plan(
        device_id: &str,
        settings: &Master3sSettings,
        plan: &SettingsApplyPlan,
    ) -> Result<SettingsApplyReport> {
        let device = find_device(device_id)?;
        apply_master3s_settings_to_device(&device, settings, plan)
    }

    pub fn listen_master3s_runtime_events(
        device_id: &str,
        event_limit: usize,
        idle_timeout: Duration,
    ) -> Result<Vec<Master3sRuntimeEvent>> {
        Master3sRuntimeEventListener::open(device_id)?.read_events(event_limit, idle_timeout)
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct ThumbWheelRuntimeInfo {
        resolution: u16,
        direction: i8,
    }

    pub struct Master3sRuntimeEventListener {
        client: HidppClient,
        slot: u8,
        features: Vec<HidppFeatureInfo>,
        thumb_wheel_info: Option<ThumbWheelRuntimeInfo>,
    }

    impl Master3sRuntimeEventListener {
        pub fn open(device_id: &str) -> Result<Self> {
            let device = find_device(device_id)?;
            Self::open_device(&device)
        }

        fn open_device(device: &DeviceInfo) -> Result<Self> {
            let paired = device.paired_device.as_ref().ok_or_else(|| {
                DogiError::Protocol(
                    "HID++ paired-device information is unavailable; cannot parse runtime notifications"
                        .to_owned(),
                )
            })?;
            let mut client = HidppClient::open(&device.path)
                .map_err(|error| DogiError::Transport(error.to_string()))?;
            let thumb_wheel_info =
                read_thumb_wheel_runtime_info(&mut client, paired.slot, &paired.features)
                    .map_err(|error| DogiError::Transport(error.to_string()))?;

            Ok(Self {
                client,
                slot: paired.slot,
                features: paired.features.clone(),
                thumb_wheel_info,
            })
        }

        pub fn read_events(
            &mut self,
            event_limit: usize,
            idle_timeout: Duration,
        ) -> Result<Vec<Master3sRuntimeEvent>> {
            if event_limit == 0 {
                return Ok(Vec::new());
            }

            let deadline = Instant::now() + idle_timeout;
            let mut events = Vec::new();

            while events.len() < event_limit {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    break;
                };
                let Some(report) = self
                    .client
                    .read_report(remaining)
                    .map_err(|error| DogiError::Transport(error.to_string()))?
                else {
                    break;
                };
                if report.get(1).copied().is_some_and(|devnumber| {
                    devnumber != self.slot && devnumber != (self.slot ^ 0xff)
                }) {
                    continue;
                }
                if let Some(mut event) =
                    parse_master3s_runtime_notification(&report, &self.features)
                {
                    if let (
                        Master3sRuntimeEvent::ThumbWheel {
                            resolution,
                            direction,
                            ..
                        },
                        Some(info),
                    ) = (&mut event, self.thumb_wheel_info)
                    {
                        *resolution = info.resolution;
                        *direction = info.direction;
                    }
                    events.push(event);
                }
            }

            Ok(events)
        }
    }

    fn read_thumb_wheel_runtime_info(
        client: &mut HidppClient,
        slot: u8,
        features: &[HidppFeatureInfo],
    ) -> io::Result<Option<ThumbWheelRuntimeInfo>> {
        let Some(index) = feature_index_in(features, HIDPP_FEATURE_THUMB_WHEEL) else {
            return Ok(None);
        };
        let Some(reply) = client.feature_request(slot, index, 0x00, &[])? else {
            return Ok(None);
        };

        Ok(decode_thumb_wheel_runtime_info(&reply))
    }

    fn decode_thumb_wheel_runtime_info(reply: &[u8]) -> Option<ThumbWheelRuntimeInfo> {
        let resolution = u16::from_be_bytes([*reply.get(2)?, *reply.get(3)?]).max(1);
        let direction = if *reply.get(4)? == 0 { -1 } else { 1 };
        Some(ThumbWheelRuntimeInfo {
            resolution,
            direction,
        })
    }

    fn read_hidraw_device(
        sysfs_path: &Path,
        hidraw_name: &str,
        depth: HidppScanDepth,
    ) -> Result<Option<DeviceInfo>> {
        let device_path = sysfs_path.join("device");
        let hid_uevent = read_uevent(&device_path.join("uevent"))?;
        let Some((bus_id, vendor_id, product_id)) =
            parse_hid_id(hid_uevent.get("HID_ID").map(String::as_str))
        else {
            return Ok(None);
        };

        let interface_path = device_path.join("..");
        let usb_device_path = device_path.join("../..");
        let interface_uevent = read_uevent(&interface_path.join("uevent"))?;
        let hidraw_dev_path = format!("/dev/{hidraw_name}");
        let sysfs_path = canonicalize_lossy(sysfs_path.to_path_buf());
        let physical_path = clean_optional_string(hid_uevent.get("HID_PHYS").map(String::as_str));
        let driver = clean_optional_string(hid_uevent.get("DRIVER").map(String::as_str));
        let name = first_non_empty(&[
            hid_uevent.get("HID_NAME").cloned(),
            read_trimmed(&usb_device_path.join("product"))?,
            Some(format!("HID device {product_id:04x}")),
        ])
        .unwrap_or_else(|| format!("HID device {product_id:04x}"));
        let manufacturer = first_non_empty(&[
            read_trimmed(&usb_device_path.join("manufacturer"))?,
            is_logitech_vendor(vendor_id).then(|| "Logitech".to_owned()),
        ]);
        let serial_number = first_non_empty(&[
            read_trimmed(&usb_device_path.join("serial"))?,
            clean_optional_string(hid_uevent.get("HID_UNIQ").map(String::as_str)),
        ]);
        let release_number = read_trimmed(&usb_device_path.join("bcdDevice"))?
            .as_deref()
            .and_then(parse_hex_u16);
        let interface_number = read_trimmed(&interface_path.join("bInterfaceNumber"))?
            .as_deref()
            .and_then(parse_hex_i32)
            .or_else(|| parse_interface_number_from_modalias(interface_uevent.get("MODALIAS")));
        let receiver_kind = infer_receiver_kind(product_id, Some(&name));
        let descriptor = fs::read(device_path.join("report_descriptor")).ok();
        let mut report_descriptor = descriptor
            .as_deref()
            .map(summarize_report_descriptor)
            .unwrap_or_default();

        if !is_logitech_vendor(vendor_id) {
            report_descriptor.hidpp_usage = None;
        }

        let hidpp_usage = report_descriptor.hidpp_usage.as_ref();
        let mut capabilities = DeviceCapabilities {
            hidpp: if report_descriptor.is_hidpp_interface() {
                CapabilityState::Supported
            } else {
                CapabilityState::Unknown
            },
            battery: CapabilityState::Unknown,
            dpi: CapabilityState::Unknown,
            button_mapping: CapabilityState::Unknown,
            onboard_profiles: CapabilityState::Unknown,
            wheel_mode: CapabilityState::Unknown,
        };
        let bus = bus_kind_from_linux_bus_id(bus_id);
        let connection = match bus {
            dogi_core::BusKind::Bluetooth => ConnectionKind::Bluetooth,
            _ => infer_connection(product_id, Some(&name), &hidraw_dev_path),
        };
        let id_seed = first_non_empty(&[
            physical_path.clone(),
            serial_number.clone(),
            Some(format!("{sysfs_path}:{hidraw_dev_path}")),
        ])
        .unwrap_or_else(|| hidraw_dev_path.clone());
        let hidraw_readwrite = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&hidraw_dev_path)
            .is_ok();
        let hidraw_readable = hidraw_readwrite || File::open(&hidraw_dev_path).is_ok();
        let access = DeviceAccess {
            sysfs_readable: true,
            hidraw_readable,
            hidraw_readwrite,
            write_policy: if report_descriptor.is_hidpp_interface() && hidraw_readwrite {
                WritePolicy::ExplicitApplyOnly
            } else {
                WritePolicy::Disabled
            },
        };
        let hidpp_probe =
            if depth != HidppScanDepth::Inventory && report_descriptor.is_hidpp_interface() {
                read_hidpp_device_probe(&hidraw_dev_path, receiver_kind, depth)?
            } else {
                HidppDeviceProbe::default()
            };
        if let Some(paired_device) = &hidpp_probe.paired_device {
            apply_hidpp_feature_capabilities(&mut capabilities, &paired_device.features);
        }
        let battery = if let Some(battery) = hidpp_probe.battery {
            battery
        } else {
            let reason = hidpp_probe.detail.unwrap_or_else(|| {
                if depth != HidppScanDepth::Inventory {
                    "battery is read through HID++ features 0x1004/0x1000/0x1001; this interface did not expose a paired-device battery"
                } else {
                    "HID++ identity, capabilities, and battery have not been queried yet"
                }
                .to_owned()
            });
            BatteryInfo::not_queried(reason)
        };
        if battery.level_percent.is_some() || battery.source == BatterySource::Hidpp {
            capabilities.battery = CapabilityState::Supported;
        }

        Ok(Some(DeviceInfo {
            id: stable_device_id(vendor_id, product_id, &id_seed),
            name,
            paired_device: hidpp_probe.paired_device,
            manufacturer,
            serial_number,
            bus,
            bus_id: Some(bus_id),
            vendor_id,
            product_id,
            release_number,
            connection,
            receiver_kind,
            path: hidraw_dev_path,
            sysfs_path,
            physical_path,
            driver,
            interface_number,
            usage_page: hidpp_usage.map(|usage| usage.usage_page),
            usage: hidpp_usage.map(|usage| usage.usage),
            access,
            battery,
            report_descriptor,
            capabilities,
        }))
    }

    fn apply_master3s_settings_to_device(
        device: &DeviceInfo,
        settings: &Master3sSettings,
        plan: &SettingsApplyPlan,
    ) -> Result<SettingsApplyReport> {
        if !device.access.hidraw_readwrite {
            return Err(DogiError::Transport(format!(
                "{} needs read/write hidraw permission for HID++ apply",
                device.path
            )));
        }

        let paired = device.paired_device.as_ref().ok_or_else(|| {
            DogiError::Protocol(
                "HID++ paired-device information is unavailable; cannot choose target slot"
                    .to_owned(),
            )
        })?;
        let mut client = HidppClient::open(&device.path)
            .map_err(|error| DogiError::Transport(error.to_string()))?;
        let settings = settings.normalized();
        let slot = paired.slot;
        let features = &paired.features;
        let mut outcomes = Vec::with_capacity(plan.steps.len());

        for step in plan.steps.iter().filter(|step| step.requires_device_write) {
            let outcome = match &step.operation {
                SettingsApplyOperation::PointerSpeed { .. } => {
                    apply_pointer_speed(&mut client, slot, features, &settings)
                }
                SettingsApplyOperation::WheelBehavior { .. } => {
                    apply_smart_shift(&mut client, slot, features, &settings)
                }
                SettingsApplyOperation::ScrollBehavior {
                    high_resolution,
                    natural,
                } => apply_hires_wheel(&mut client, slot, features, *high_resolution, *natural),
                SettingsApplyOperation::ThumbWheel { .. } => {
                    apply_thumb_wheel(&mut client, slot, features, &settings)
                }
                SettingsApplyOperation::ButtonMapping { button, .. } => apply_button_diversion(
                    &mut client,
                    slot,
                    features,
                    &ButtonBinding {
                        button: *button,
                        action: settings.button_action(*button),
                    },
                ),
                SettingsApplyOperation::LocalRuntime { .. }
                | SettingsApplyOperation::AppProfile { .. } => continue,
            };
            outcomes.push(outcome);
        }

        Ok(SettingsApplyReport {
            device_id: device.id.clone(),
            profile_name: settings.profile_name,
            outcomes,
        })
    }

    fn apply_pointer_speed(
        client: &mut HidppClient,
        slot: u8,
        features: &[HidppFeatureInfo],
        settings: &Master3sSettings,
    ) -> SettingsApplyOutcome {
        let title = format!("Set pointer speed to {}%", settings.pointer_speed_percent);
        let Some(index) = feature_index_in(features, HIDPP_FEATURE_POINTER_SPEED) else {
            return unsupported_outcome(
                title,
                HidppFeature::PointerSpeed,
                "POINTER_SPEED feature is not available",
            );
        };
        let value = pointer_speed_hidpp_value(settings.pointer_speed_percent);
        let payload = value.to_be_bytes();

        write_outcome(
            client,
            slot,
            WriteRequest {
                feature_index: index,
                function: 0x10,
                payload: &payload,
                title,
                feature: HidppFeature::PointerSpeed,
                detail: format!("raw multiplier {value}"),
            },
        )
    }

    fn apply_smart_shift(
        client: &mut HidppClient,
        slot: u8,
        features: &[HidppFeatureInfo],
        settings: &Master3sSettings,
    ) -> SettingsApplyOutcome {
        let title = if settings.smart_shift_enabled
            && settings.ratchet_mode == WheelRatchetMode::SmartShift
        {
            format!(
                "Enable SmartShift at threshold {}",
                settings.smart_shift_threshold
            )
        } else {
            format!("Set wheel mode to {}", settings.ratchet_mode.label())
        };
        let Some((index, function)) = smart_shift_feature_index(features) else {
            return unsupported_outcome(
                title,
                HidppFeature::SmartShift,
                "SMART_SHIFT feature is not available",
            );
        };
        let payload = smart_shift_payload(settings);

        write_outcome(
            client,
            slot,
            WriteRequest {
                feature_index: index,
                function,
                payload: &payload,
                title,
                feature: HidppFeature::SmartShift,
                detail: format!("payload {}", format_hex_bytes(&payload)),
            },
        )
    }

    fn apply_hires_wheel(
        client: &mut HidppClient,
        slot: u8,
        features: &[HidppFeatureInfo],
        high_resolution: Option<bool>,
        natural: Option<bool>,
    ) -> SettingsApplyOutcome {
        let title = match (high_resolution, natural) {
            (Some(high_resolution), Some(natural)) => format!(
                "{} smooth scrolling and use the {} direction",
                if high_resolution { "Enable" } else { "Disable" },
                if natural { "natural" } else { "standard" }
            ),
            (Some(true), None) => "Enable smooth scrolling".to_owned(),
            (Some(false), None) => "Disable smooth scrolling".to_owned(),
            (None, Some(true)) => "Use the natural scroll direction".to_owned(),
            (None, Some(false)) => "Use the standard scroll direction".to_owned(),
            (None, None) => "Keep the current scroll behavior".to_owned(),
        };
        let Some(index) = feature_index_in(features, HIDPP_FEATURE_HIRES_WHEEL) else {
            return unsupported_outcome(
                title,
                HidppFeature::HiresWheel,
                "HIRES_WHEEL feature is not available",
            );
        };
        let current = match client.feature_request(slot, index, 0x10, &[]) {
            Ok(Some(reply)) => reply,
            Ok(None) => {
                return failed_outcome(
                    title,
                    HidppFeature::HiresWheel,
                    "HIRES_WHEEL mode read did not return a reply",
                );
            }
            Err(error) => {
                return failed_outcome(
                    title,
                    HidppFeature::HiresWheel,
                    format!("HIRES_WHEEL mode read failed: {error}"),
                );
            }
        };
        let Some(current_mode) = current.first().copied() else {
            return failed_outcome(
                title,
                HidppFeature::HiresWheel,
                "HIRES_WHEEL mode reply was empty",
            );
        };
        let new_mode = merge_hires_wheel_flags(current_mode, high_resolution, natural);
        let payload = [new_mode];

        write_outcome(
            client,
            slot,
            WriteRequest {
                feature_index: index,
                function: 0x20,
                payload: &payload,
                title,
                feature: HidppFeature::HiresWheel,
                detail: format!("mode 0x{current_mode:02X} -> 0x{new_mode:02X}"),
            },
        )
    }

    fn apply_thumb_wheel(
        client: &mut HidppClient,
        slot: u8,
        features: &[HidppFeatureInfo],
        settings: &Master3sSettings,
    ) -> SettingsApplyOutcome {
        let title = if settings.thumb_wheel == ThumbWheelMode::HorizontalScroll
            && settings.thumb_wheel_speed_percent == dogi_core::DEFAULT_THUMB_WHEEL_SPEED_PERCENT
        {
            "Use native horizontal scrolling".to_owned()
        } else if settings.thumb_wheel == ThumbWheelMode::HorizontalScroll {
            format!(
                "Route thumb wheel through dogi for horizontal scrolling at {}%",
                settings.thumb_wheel_speed_percent
            )
        } else {
            format!(
                "Route thumb wheel through dogi for {}",
                settings.thumb_wheel.label()
            )
        };
        let target_diverted =
            thumb_wheel_diversion_target(settings.thumb_wheel, settings.thumb_wheel_speed_percent);
        let Some(index) = feature_index_in(features, HIDPP_FEATURE_THUMB_WHEEL) else {
            return unsupported_outcome(
                title,
                HidppFeature::ThumbWheel,
                "THUMB_WHEEL feature is not available",
            );
        };
        let current = match client.feature_request(slot, index, 0x10, &[]) {
            Ok(Some(reply)) => reply,
            Ok(None) => {
                return failed_outcome(
                    title,
                    HidppFeature::ThumbWheel,
                    "THUMB_WHEEL mode read did not return a reply",
                );
            }
            Err(error) => {
                return failed_outcome(
                    title,
                    HidppFeature::ThumbWheel,
                    format!("THUMB_WHEEL mode read failed: {error}"),
                );
            }
        };
        let Some(payload) = merge_thumb_wheel_diversion(&current, target_diverted) else {
            return failed_outcome(
                title,
                HidppFeature::ThumbWheel,
                "THUMB_WHEEL mode reply did not include two state bytes",
            );
        };

        write_outcome(
            client,
            slot,
            WriteRequest {
                feature_index: index,
                function: 0x20,
                payload: &payload,
                title,
                feature: HidppFeature::ThumbWheel,
                detail: format!("mode {}", format_hex_bytes(&payload)),
            },
        )
    }

    fn apply_button_diversion(
        client: &mut HidppClient,
        slot: u8,
        features: &[HidppFeatureInfo],
        binding: &ButtonBinding,
    ) -> SettingsApplyOutcome {
        let should_divert = button_action_requires_runtime(binding.button, binding.action);
        let raw_xy = binding.action == dogi_core::ButtonAction::Gestures;
        let title = if should_divert {
            format!(
                "Divert {} for {}",
                binding.button.label(),
                binding.action.label()
            )
        } else {
            format!("Use native reporting for {}", binding.button.label())
        };
        let Some(index) = feature_index_in(features, HIDPP_FEATURE_REPROG_CONTROLS_V4) else {
            return unsupported_outcome(
                title,
                HidppFeature::ReprogrammableControls,
                "REPROG_CONTROLS_V4 feature is not available",
            );
        };
        let control_id = master3s_button_control_id(binding.button);
        let control = match read_reprog_control_info(client, slot, index, control_id) {
            Ok(Some(control)) => control,
            Ok(None) => {
                return unsupported_outcome(
                    title,
                    HidppFeature::ReprogrammableControls,
                    format!("control 0x{control_id:04X} is not reported by REPROG_CONTROLS_V4"),
                );
            }
            Err(error) => {
                return failed_outcome(
                    title,
                    HidppFeature::ReprogrammableControls,
                    format!("REPROG_CONTROLS_V4 query failed: {error}"),
                );
            }
        };

        if should_divert
            && (control.flags & REPROG_KEY_FLAG_DIVERTABLE == 0
                || control.flags & REPROG_KEY_FLAG_VIRTUAL != 0)
        {
            return unsupported_outcome(
                title,
                HidppFeature::ReprogrammableControls,
                format!("control 0x{control_id:04X} is not a physical divertable control"),
            );
        }

        if raw_xy && control.flags & REPROG_KEY_FLAG_RAW_XY == 0 {
            return unsupported_outcome(
                title,
                HidppFeature::ReprogrammableControls,
                format!("control 0x{control_id:04X} does not support raw movement reporting"),
            );
        }

        let payload = reprogrammable_control_diversion_payload(control_id, should_divert, raw_xy);
        write_outcome(
            client,
            slot,
            WriteRequest {
                feature_index: index,
                function: 0x30,
                payload: &payload,
                title,
                feature: HidppFeature::ReprogrammableControls,
                detail: format!(
                    "control 0x{control_id:04X} {}",
                    if should_divert {
                        "diverted to HID++ runtime"
                    } else {
                        "restored to native reporting"
                    }
                ),
            },
        )
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct ReprogControlInfo {
        control_id: u16,
        flags: u16,
    }

    fn read_reprog_control_info(
        client: &mut HidppClient,
        slot: u8,
        feature_index: u8,
        control_id: u16,
    ) -> io::Result<Option<ReprogControlInfo>> {
        let Some(count_reply) = client.feature_request(slot, feature_index, 0x00, &[])? else {
            return Ok(None);
        };
        let Some(count) = count_reply.first().copied() else {
            return Ok(None);
        };

        for control_index in 0..count {
            let Some(reply) =
                client.feature_request(slot, feature_index, 0x10, &[control_index])?
            else {
                continue;
            };
            let Some(info) = parse_reprog_control_info(&reply) else {
                continue;
            };
            if info.control_id == control_id {
                return Ok(Some(info));
            }
        }

        Ok(None)
    }

    fn parse_reprog_control_info(reply: &[u8]) -> Option<ReprogControlInfo> {
        let control_id = u16::from_be_bytes([*reply.first()?, *reply.get(1)?]);
        let flags_1 = u16::from(*reply.get(4)?);
        let flags_2 = u16::from(reply.get(8).copied().unwrap_or_default());
        Some(ReprogControlInfo {
            control_id,
            flags: flags_1 | (flags_2 << 8),
        })
    }

    fn reprogrammable_control_diversion_payload(
        control_id: u16,
        diverted: bool,
        raw_xy: bool,
    ) -> [u8; 5] {
        let [high, low] = control_id.to_be_bytes();
        let flags = REPROG_MAPPING_FLAG_DIVERTED_VALID
            | REPROG_MAPPING_FLAG_RAW_XY_VALID
            | if diverted {
                REPROG_MAPPING_FLAG_DIVERTED
            } else {
                0
            }
            | if raw_xy {
                REPROG_MAPPING_FLAG_RAW_XY
            } else {
                0
            };
        [high, low, flags, 0x00, 0x00]
    }

    struct WriteRequest<'a> {
        feature_index: u8,
        function: u8,
        payload: &'a [u8],
        title: String,
        feature: HidppFeature,
        detail: String,
    }

    fn write_outcome(
        client: &mut HidppClient,
        slot: u8,
        request: WriteRequest<'_>,
    ) -> SettingsApplyOutcome {
        match client.feature_request(
            slot,
            request.feature_index,
            request.function,
            request.payload,
        ) {
            Ok(Some(_)) => SettingsApplyOutcome {
                title: request.title,
                feature: request.feature,
                status: SettingsApplyStatus::Applied,
                detail: Some(request.detail),
            },
            Ok(None) => failed_outcome(
                request.title,
                request.feature,
                "HID++ write did not return a reply",
            ),
            Err(error) => failed_outcome(
                request.title,
                request.feature,
                format!("HID++ write failed: {error}"),
            ),
        }
    }

    fn unsupported_outcome(
        title: String,
        feature: HidppFeature,
        detail: impl Into<String>,
    ) -> SettingsApplyOutcome {
        SettingsApplyOutcome {
            title,
            feature,
            status: SettingsApplyStatus::Unsupported,
            detail: Some(detail.into()),
        }
    }

    fn failed_outcome(
        title: String,
        feature: HidppFeature,
        detail: impl Into<String>,
    ) -> SettingsApplyOutcome {
        SettingsApplyOutcome {
            title,
            feature,
            status: SettingsApplyStatus::Failed,
            detail: Some(detail.into()),
        }
    }

    fn pointer_speed_hidpp_value(percent: u8) -> u16 {
        let scaled = (u32::from(percent) * 256 + 50) / 100;
        scaled.clamp(0x002e, 0x01ff) as u16
    }

    fn smart_shift_feature_index(features: &[HidppFeatureInfo]) -> Option<(u8, u8)> {
        feature_index_in(features, HIDPP_FEATURE_SMART_SHIFT_ENHANCED)
            .map(|index| (index, 0x20))
            .or_else(|| {
                feature_index_in(features, HIDPP_FEATURE_SMART_SHIFT).map(|index| (index, 0x10))
            })
    }

    fn smart_shift_payload(settings: &Master3sSettings) -> Vec<u8> {
        if !settings.smart_shift_enabled || settings.ratchet_mode == WheelRatchetMode::Ratchet {
            return vec![2];
        }

        if settings.ratchet_mode == WheelRatchetMode::FreeSpin {
            return vec![1];
        }

        let threshold = if settings.smart_shift_threshold >= 50 {
            255
        } else {
            settings.smart_shift_threshold
        };
        vec![0, threshold]
    }

    fn merge_hires_wheel_flags(
        current: u8,
        high_resolution: Option<bool>,
        natural_scroll: Option<bool>,
    ) -> u8 {
        let mut value = current;
        if let Some(high_resolution) = high_resolution {
            set_flag(&mut value, 0x02, high_resolution);
        }
        if let Some(natural_scroll) = natural_scroll {
            set_flag(&mut value, 0x04, natural_scroll);
        }
        value
    }

    fn thumb_wheel_diversion_target(mode: ThumbWheelMode, speed_percent: u16) -> bool {
        match mode {
            ThumbWheelMode::HorizontalScroll => {
                speed_percent != dogi_core::DEFAULT_THUMB_WHEEL_SPEED_PERCENT
            }
            ThumbWheelMode::TabSwitch
            | ThumbWheelMode::Zoom
            | ThumbWheelMode::Volume
            | ThumbWheelMode::Disabled => true,
        }
    }

    fn merge_thumb_wheel_diversion(current: &[u8], diverted: bool) -> Option<Vec<u8>> {
        let first = current.first().copied()?;
        let second = current.get(1).copied()?;
        let mut mode = first;
        set_flag(&mut mode, 0x01, diverted);
        Some(vec![mode, second])
    }

    fn set_flag(value: &mut u8, mask: u8, enabled: bool) {
        if enabled {
            *value |= mask;
        } else {
            *value &= !mask;
        }
    }

    fn format_hex_bytes(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn read_uevent(path: &Path) -> Result<HashMap<String, String>> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(error) => return Err(DogiError::Transport(error.to_string())),
        };

        Ok(contents
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect())
    }

    fn read_trimmed(path: &Path) -> Result<Option<String>> {
        match fs::read_to_string(path) {
            Ok(contents) => Ok(clean_optional_string(Some(contents.as_str()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(None),
            Err(error) => Err(DogiError::Transport(error.to_string())),
        }
    }

    #[derive(Debug, Default)]
    struct HidppDeviceProbe {
        paired_device: Option<PairedDeviceInfo>,
        battery: Option<BatteryInfo>,
        detail: Option<String>,
    }

    #[derive(Clone)]
    struct CachedHidppProbe {
        receiver_kind: Option<ReceiverKind>,
        depth: HidppScanDepth,
        paired_device: PairedDeviceInfo,
    }

    static HIDPP_PROBE_CACHE: OnceLock<Mutex<HashMap<String, CachedHidppProbe>>> = OnceLock::new();

    #[derive(Clone, Debug, Default)]
    struct ReceiverPairingInfo {
        name: Option<String>,
        kind: Option<String>,
        wpid: Option<String>,
        serial: Option<String>,
        slot_state_known: bool,
    }

    impl ReceiverPairingInfo {
        fn identifies_device(&self) -> bool {
            self.name.is_some()
                || self.kind.is_some()
                || self.wpid.is_some()
                || self.serial.is_some()
        }
    }

    trait HidppProbeClient {
        fn read_receiver_pairing_device(
            &mut self,
            slot: u8,
            receiver_kind: Option<ReceiverKind>,
        ) -> io::Result<ReceiverPairingInfo>;

        fn ping(&mut self, devnumber: u8) -> io::Result<Option<HidppProtocolVersion>>;

        fn read_paired_device(
            &mut self,
            devnumber: u8,
            protocol: HidppProtocolVersion,
            receiver_pairing: &ReceiverPairingInfo,
        ) -> io::Result<(PairedDeviceInfo, Option<BatteryInfo>)>;

        fn read_battery(
            &mut self,
            devnumber: u8,
            features: &[HidppFeatureInfo],
        ) -> io::Result<Option<BatteryInfo>>;
    }

    fn read_hidpp_device_probe(
        path: &str,
        receiver_kind: Option<ReceiverKind>,
        depth: HidppScanDepth,
    ) -> Result<HidppDeviceProbe> {
        let mut client = match HidppClient::open_with_depth(path, depth) {
            Ok(client) => client,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return Ok(HidppDeviceProbe {
                    paired_device: None,
                    battery: None,
                    detail: Some(format!(
                        "HID++ paired-device query requires read/write access to {path}"
                    )),
                });
            }
            Err(error) => {
                return Ok(HidppDeviceProbe {
                    paired_device: None,
                    battery: None,
                    detail: Some(format!(
                        "HID++ paired-device query failed to open {path}: {error}"
                    )),
                });
            }
        };

        if let Some(cached) = cached_hidpp_probe(path, receiver_kind, depth)
            && let Some(probe) =
                refresh_cached_hidpp_probe(&mut client, cached, receiver_kind, std::thread::sleep)
        {
            return Ok(probe);
        }

        let probe = probe_hidpp_device(&mut client, receiver_kind, std::thread::sleep);
        remember_hidpp_probe(path, receiver_kind, depth, &probe);
        Ok(probe)
    }

    fn hidpp_probe_cache() -> &'static Mutex<HashMap<String, CachedHidppProbe>> {
        HIDPP_PROBE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn cached_hidpp_probe(
        path: &str,
        receiver_kind: Option<ReceiverKind>,
        depth: HidppScanDepth,
    ) -> Option<CachedHidppProbe> {
        hidpp_probe_cache()
            .lock()
            .ok()?
            .get(path)
            .filter(|cached| cached.receiver_kind == receiver_kind && cached.depth >= depth)
            .cloned()
    }

    fn remember_hidpp_probe(
        path: &str,
        receiver_kind: Option<ReceiverKind>,
        depth: HidppScanDepth,
        probe: &HidppDeviceProbe,
    ) {
        if receiver_kind.is_none() {
            return;
        }
        let Some(paired_device) = probe
            .paired_device
            .as_ref()
            .filter(|paired| !paired.features.is_empty())
        else {
            return;
        };
        if let Ok(mut cache) = hidpp_probe_cache().lock() {
            cache.insert(
                path.to_owned(),
                CachedHidppProbe {
                    receiver_kind,
                    depth,
                    paired_device: paired_device.clone(),
                },
            );
        }
    }

    fn refresh_cached_hidpp_probe(
        client: &mut HidppClient,
        cached: CachedHidppProbe,
        receiver_kind: Option<ReceiverKind>,
        mut pause: impl FnMut(Duration),
    ) -> Option<HidppDeviceProbe> {
        let mut paired_device = cached.paired_device;
        if receiver_kind.is_some() {
            let current = client
                .read_receiver_pairing_device(paired_device.slot, receiver_kind)
                .ok()?;
            if current.slot_state_known && !current.identifies_device() {
                return None;
            }
            if current
                .wpid
                .as_deref()
                .zip(paired_device.wpid.as_deref())
                .is_some_and(|(current, cached)| current != cached)
            {
                return None;
            }
            merge_receiver_pairing_info(&mut paired_device, current);
        }

        let protocol = retry_probe(true, || client.ping(paired_device.slot), &mut pause)
            .ok()
            .flatten()?;
        paired_device.protocol = Some(protocol);

        let battery = retry_probe(
            true,
            || {
                client
                    .read_battery_from_features(paired_device.slot, &paired_device.features)
                    .map(|battery| battery.map(Some))
            },
            &mut pause,
        )
        .ok()
        .flatten()
        .flatten();
        let detail = battery.is_none().then(|| {
            format!(
                "HID++ device slot {} did not return a supported battery value after {} attempts",
                paired_device.slot,
                HIDPP_PROBE_RETRY_DELAYS.len() + 1
            )
        });

        Some(HidppDeviceProbe {
            paired_device: Some(paired_device),
            battery,
            detail,
        })
    }

    fn probe_hidpp_device(
        client: &mut impl HidppProbeClient,
        receiver_kind: Option<ReceiverKind>,
        mut pause: impl FnMut(Duration),
    ) -> HidppDeviceProbe {
        let mut detail = None;
        for &devnumber in hidpp_candidate_devnumbers(receiver_kind) {
            let receiver_pairing = if receiver_kind.is_some() {
                match client.read_receiver_pairing_device(devnumber, receiver_kind) {
                    Ok(info) => info,
                    Err(error) => {
                        detail = Some(format!(
                            "HID++ receiver pairing query failed on device slot {devnumber}: {error}"
                        ));
                        ReceiverPairingInfo::default()
                    }
                }
            } else {
                ReceiverPairingInfo::default()
            };

            let slot_is_known = receiver_pairing.identifies_device();
            if receiver_kind.is_some() && receiver_pairing.slot_state_known && !slot_is_known {
                continue;
            }
            let retry_ping =
                slot_is_known || (receiver_kind.is_none() && devnumber == HIDPP_RECEIVER_DEVNUMBER);
            let ping = retry_probe(retry_ping, || client.ping(devnumber), &mut pause);

            match ping {
                Ok(Some(protocol)) => {
                    match read_paired_device_with_retry(
                        client,
                        devnumber,
                        protocol,
                        &receiver_pairing,
                        &mut pause,
                    ) {
                        Ok((mut paired_device, battery)) => {
                            merge_receiver_pairing_info(&mut paired_device, receiver_pairing);
                            let detail = battery.is_none().then(|| {
                                format!(
                                    "HID++ device slot {devnumber} did not return a supported battery value after {} attempts",
                                    HIDPP_PROBE_RETRY_DELAYS.len() + 1
                                )
                            });
                            return HidppDeviceProbe {
                                paired_device: Some(paired_device),
                                battery,
                                detail,
                            };
                        }
                        Err(error) => {
                            detail = Some(format!(
                                "HID++ paired-device query failed on device slot {devnumber}: {error}"
                            ));
                        }
                    }
                }
                Ok(None) => {
                    if let Some(paired_device) =
                        paired_device_from_receiver_pairing(devnumber, receiver_pairing)
                    {
                        return HidppDeviceProbe {
                            paired_device: Some(paired_device),
                            battery: None,
                            detail: Some(format!(
                                "HID++ receiver pairing table identified device slot {devnumber}; the device did not answer after {} attempts",
                                HIDPP_PROBE_RETRY_DELAYS.len() + 1
                            )),
                        };
                    }
                }
                Err(error) => {
                    detail = Some(format!(
                        "HID++ ping failed on device slot {devnumber}: {error}"
                    ));
                    if let Some(paired_device) =
                        paired_device_from_receiver_pairing(devnumber, receiver_pairing)
                    {
                        return HidppDeviceProbe {
                            paired_device: Some(paired_device),
                            battery: None,
                            detail,
                        };
                    }
                }
            }
        }

        HidppDeviceProbe {
            paired_device: None,
            battery: None,
            detail: Some(detail.unwrap_or_else(|| {
                "HID++ paired-device query did not find a responding paired device".to_owned()
            })),
        }
    }

    fn retry_probe<T>(
        retry: bool,
        mut operation: impl FnMut() -> io::Result<Option<T>>,
        pause: &mut impl FnMut(Duration),
    ) -> io::Result<Option<T>> {
        let retry_delays = if retry {
            HIDPP_PROBE_RETRY_DELAYS.as_slice()
        } else {
            &[]
        };

        for retry_delay in retry_delays
            .iter()
            .copied()
            .map(Some)
            .chain(std::iter::once(None))
        {
            match operation() {
                Ok(Some(value)) => return Ok(Some(value)),
                Ok(None) => match retry_delay {
                    Some(delay) => pause(delay),
                    None => return Ok(None),
                },
                Err(error) if is_retryable_probe_error(&error) => match retry_delay {
                    Some(delay) => pause(delay),
                    None => return Err(error),
                },
                Err(error) => return Err(error),
            }
        }

        Ok(None)
    }

    fn read_paired_device_with_retry(
        client: &mut impl HidppProbeClient,
        devnumber: u8,
        protocol: HidppProtocolVersion,
        receiver_pairing: &ReceiverPairingInfo,
        pause: &mut impl FnMut(Duration),
    ) -> io::Result<(PairedDeviceInfo, Option<BatteryInfo>)> {
        let mut best_incomplete: Option<(PairedDeviceInfo, Option<BatteryInfo>)> = None;
        let mut last_retryable_error = None;

        for attempt in 0..=HIDPP_PROBE_RETRY_DELAYS.len() {
            let cached_features = best_incomplete
                .as_ref()
                .map(|(paired, _)| paired.features.as_slice())
                .filter(|features| {
                    features.iter().any(|feature| {
                        matches!(
                            feature.feature_id,
                            HIDPP_FEATURE_UNIFIED_BATTERY
                                | HIDPP_FEATURE_BATTERY_STATUS
                                | HIDPP_FEATURE_BATTERY_VOLTAGE
                        )
                    })
                });

            if let Some(features) = cached_features {
                match client.read_battery(devnumber, features) {
                    Ok(Some(battery)) => {
                        if let Some((paired_device, _)) = best_incomplete.take() {
                            return Ok((paired_device, Some(battery)));
                        }
                    }
                    Ok(None) => {}
                    Err(error) if is_retryable_probe_error(&error) => {
                        last_retryable_error = Some(error);
                    }
                    Err(error) => return best_incomplete.ok_or(error),
                }
            } else {
                match client.read_paired_device(devnumber, protocol, receiver_pairing) {
                    Ok((paired_device, Some(battery))) => {
                        return Ok((paired_device, Some(battery)));
                    }
                    Ok(candidate @ (.., None)) => {
                        let replace = best_incomplete.as_ref().is_none_or(
                            |(current, _): &(PairedDeviceInfo, Option<BatteryInfo>)| {
                                candidate.0.feature_count > current.feature_count
                            },
                        );
                        if replace {
                            best_incomplete = Some(candidate);
                        }
                    }
                    Err(error) if is_retryable_probe_error(&error) => {
                        last_retryable_error = Some(error);
                    }
                    Err(error) => return best_incomplete.ok_or(error),
                }
            }

            if let Some(delay) = HIDPP_PROBE_RETRY_DELAYS.get(attempt).copied() {
                pause(delay);
            }
        }

        if let Some(best_incomplete) = best_incomplete {
            return Ok(best_incomplete);
        }

        Err(last_retryable_error.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "HID++ paired-device probe exhausted its retry budget",
            )
        }))
    }

    fn is_retryable_probe_error(error: &io::Error) -> bool {
        matches!(
            error.kind(),
            io::ErrorKind::Interrupted | io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        )
    }

    fn apply_hidpp_feature_capabilities(
        capabilities: &mut DeviceCapabilities,
        features: &[HidppFeatureInfo],
    ) {
        if has_feature(
            features,
            &[
                HIDPP_FEATURE_UNIFIED_BATTERY,
                HIDPP_FEATURE_BATTERY_STATUS,
                HIDPP_FEATURE_BATTERY_VOLTAGE,
            ],
        ) {
            capabilities.battery = CapabilityState::Supported;
        }

        if has_feature(
            features,
            &[
                HIDPP_FEATURE_POINTER_SPEED,
                HIDPP_FEATURE_MOUSE_POINTER,
                HIDPP_FEATURE_ADJUSTABLE_DPI,
                HIDPP_FEATURE_EXTENDED_ADJUSTABLE_DPI,
            ],
        ) {
            capabilities.dpi = CapabilityState::Supported;
        }

        if has_feature(
            features,
            &[
                HIDPP_FEATURE_SMART_SHIFT,
                HIDPP_FEATURE_SMART_SHIFT_ENHANCED,
                HIDPP_FEATURE_HIRES_WHEEL,
                HIDPP_FEATURE_THUMB_WHEEL,
            ],
        ) {
            capabilities.wheel_mode = CapabilityState::Supported;
        }

        if has_feature(
            features,
            &[
                HIDPP_FEATURE_REPROG_CONTROLS,
                HIDPP_FEATURE_REPROG_CONTROLS_V2,
                HIDPP_FEATURE_REPROG_CONTROLS_V3,
                HIDPP_FEATURE_REPROG_CONTROLS_V4,
            ],
        ) {
            capabilities.button_mapping = CapabilityState::Supported;
        }

        if has_feature(
            features,
            &[
                HIDPP_FEATURE_ONBOARD_PROFILES,
                HIDPP_FEATURE_PROFILE_MANAGEMENT,
            ],
        ) {
            capabilities.onboard_profiles = CapabilityState::Supported;
        }
    }

    fn has_feature(features: &[HidppFeatureInfo], feature_ids: &[u16]) -> bool {
        features
            .iter()
            .any(|feature| feature_ids.contains(&feature.feature_id))
    }

    fn feature_index_in(features: &[HidppFeatureInfo], feature_id: u16) -> Option<u8> {
        features
            .iter()
            .find(|feature| feature.feature_id == feature_id)
            .map(|feature| feature.index)
    }

    fn merge_receiver_pairing_info(
        paired_device: &mut PairedDeviceInfo,
        receiver_pairing: ReceiverPairingInfo,
    ) {
        if paired_device.name.is_none() {
            paired_device.name = receiver_pairing.name;
        }
        if paired_device.kind.is_none() {
            paired_device.kind = receiver_pairing.kind;
        }
        if paired_device.wpid.is_none() {
            paired_device.wpid = receiver_pairing.wpid;
        }
    }

    fn paired_device_from_receiver_pairing(
        slot: u8,
        receiver_pairing: ReceiverPairingInfo,
    ) -> Option<PairedDeviceInfo> {
        if receiver_pairing.name.is_none()
            && receiver_pairing.kind.is_none()
            && receiver_pairing.wpid.is_none()
        {
            return None;
        }

        Some(PairedDeviceInfo {
            slot,
            name: receiver_pairing.name,
            kind: receiver_pairing.kind,
            wpid: receiver_pairing.wpid,
            protocol: None,
            unit_id: None,
            model_id: None,
            feature_count: 0,
            features: Vec::new(),
        })
    }

    fn unifying_pairing_info_from_register(reply: &[u8]) -> ReceiverPairingInfo {
        ReceiverPairingInfo {
            name: None,
            kind: reply
                .get(7)
                .copied()
                .and_then(|value| hidpp_device_kind(value & 0x0f)),
            wpid: reply
                .get(3..5)
                .map(hex_upper)
                .filter(|value| !value.is_empty()),
            serial: None,
            slot_state_known: true,
        }
    }

    fn bolt_pairing_info_from_register(reply: &[u8]) -> ReceiverPairingInfo {
        ReceiverPairingInfo {
            name: None,
            kind: reply
                .get(1)
                .copied()
                .and_then(|value| hidpp_device_kind(value & 0x0f)),
            wpid: match (reply.get(3), reply.get(2)) {
                (Some(high), Some(low)) => Some(hex_upper(&[*high, *low])),
                _ => None,
            },
            serial: reply
                .get(4..8)
                .map(hex_upper)
                .filter(|value| !value.is_empty()),
            slot_state_known: true,
        }
    }

    fn unifying_device_name_from_register(reply: &[u8]) -> Option<String> {
        let length = usize::from(*reply.get(1)?);
        if length == 0 {
            return None;
        }

        clean_hidpp_string(reply.get(2..2 + length.min(reply.len().saturating_sub(2)))?)
    }

    fn bolt_device_name_from_register(reply: &[u8]) -> Option<String> {
        let length = usize::from(*reply.get(2)?).min(14);
        if length == 0 {
            return None;
        }

        clean_hidpp_string(reply.get(3..3 + length.min(reply.len().saturating_sub(3)))?)
    }

    fn hidpp_feature_name(feature_id: u16) -> String {
        match feature_id {
            HIDPP_FEATURE_ROOT => "ROOT",
            HIDPP_FEATURE_FEATURE_SET => "FEATURE_SET",
            HIDPP_FEATURE_DEVICE_FW_VERSION => "DEVICE_FW_VERSION",
            HIDPP_FEATURE_DEVICE_NAME => "DEVICE_NAME",
            HIDPP_FEATURE_BATTERY_STATUS => "BATTERY_STATUS",
            HIDPP_FEATURE_BATTERY_VOLTAGE => "BATTERY_VOLTAGE",
            HIDPP_FEATURE_UNIFIED_BATTERY => "UNIFIED_BATTERY",
            HIDPP_FEATURE_REPROG_CONTROLS => "REPROG_CONTROLS",
            HIDPP_FEATURE_REPROG_CONTROLS_V2 => "REPROG_CONTROLS_V2",
            HIDPP_FEATURE_REPROG_CONTROLS_V3 => "REPROG_CONTROLS_V3",
            HIDPP_FEATURE_REPROG_CONTROLS_V4 => "REPROG_CONTROLS_V4",
            HIDPP_FEATURE_SMART_SHIFT => "SMART_SHIFT",
            HIDPP_FEATURE_SMART_SHIFT_ENHANCED => "SMART_SHIFT_ENHANCED",
            HIDPP_FEATURE_HIRES_WHEEL => "HIRES_WHEEL",
            HIDPP_FEATURE_THUMB_WHEEL => "THUMB_WHEEL",
            HIDPP_FEATURE_MOUSE_POINTER => "MOUSE_POINTER",
            HIDPP_FEATURE_ADJUSTABLE_DPI => "ADJUSTABLE_DPI",
            HIDPP_FEATURE_EXTENDED_ADJUSTABLE_DPI => "EXTENDED_ADJUSTABLE_DPI",
            HIDPP_FEATURE_POINTER_SPEED => "POINTER_SPEED",
            HIDPP_FEATURE_ONBOARD_PROFILES => "ONBOARD_PROFILES",
            HIDPP_FEATURE_PROFILE_MANAGEMENT => "PROFILE_MANAGEMENT",
            _ => return format!("UNKNOWN_{feature_id:04X}"),
        }
        .to_owned()
    }

    fn hidpp_device_kind(value: u8) -> Option<String> {
        let kind = match value {
            0x00 => "unknown",
            0x01 => "keyboard",
            0x02 => "remote-control",
            0x03 => "numpad",
            0x04 => "mouse",
            0x05 => "touchpad",
            0x06 => "trackball",
            0x07 => "presenter",
            0x08 => "receiver",
            _ => return None,
        };

        Some(kind.to_owned())
    }

    fn hex_upper(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join("")
    }

    fn clean_hidpp_string(value: &[u8]) -> Option<String> {
        let text = String::from_utf8(value.to_vec()).ok()?;
        let trimmed = text.trim_matches(char::from(0)).trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }

    fn hidpp_candidate_devnumbers(receiver_kind: Option<ReceiverKind>) -> &'static [u8] {
        const RECEIVER_SLOTS: &[u8] = &[1, 2, 3, 4, 5, 6, 7];
        const DIRECT_OR_UNKNOWN: &[u8] = &[0xff, 1, 2, 3, 4, 5, 6, 7];

        if receiver_kind.is_some() {
            RECEIVER_SLOTS
        } else {
            DIRECT_OR_UNKNOWN
        }
    }

    struct HidppClient {
        file: File,
        scan_depth: HidppScanDepth,
    }

    impl HidppClient {
        fn open(path: &str) -> io::Result<Self> {
            Self::open_with_depth(path, HidppScanDepth::Full)
        }

        fn open_with_depth(path: &str, scan_depth: HidppScanDepth) -> io::Result<Self> {
            let file = OpenOptions::new().read(true).write(true).open(path)?;
            set_nonblocking(&file)?;
            let mut client = Self { file, scan_depth };
            client.drain_input()?;
            Ok(client)
        }

        fn ping(&mut self, devnumber: u8) -> io::Result<Option<HidppProtocolVersion>> {
            let mark = 0xa5 ^ devnumber;
            let Some(reply) =
                self.request(devnumber, 0x0010, &[0x00, 0x00, mark], HIDPP_PING_TIMEOUT)?
            else {
                return Ok(None);
            };

            if reply.len() >= 3 && reply[2] == mark {
                Ok(Some(HidppProtocolVersion {
                    major: reply[0],
                    minor: reply[1],
                }))
            } else {
                Ok(None)
            }
        }

        fn read_paired_device(
            &mut self,
            devnumber: u8,
            protocol: HidppProtocolVersion,
            receiver_pairing: &ReceiverPairingInfo,
        ) -> io::Result<(PairedDeviceInfo, Option<BatteryInfo>)> {
            let features = self.enumerate_features(devnumber, receiver_pairing)?;
            let name = if receiver_pairing.name.is_some() {
                None
            } else {
                self.read_device_name(devnumber, &features)?
            };
            let kind = if receiver_pairing.kind.is_some() {
                None
            } else {
                self.read_device_kind(devnumber, &features)?
            };
            let (unit_id, model_id) = self.read_device_ids(devnumber, &features)?;
            let battery = self.read_battery_from_features(devnumber, &features)?;
            let feature_count = features.len();

            Ok((
                PairedDeviceInfo {
                    slot: devnumber,
                    name,
                    kind,
                    wpid: None,
                    protocol: Some(protocol),
                    unit_id,
                    model_id,
                    feature_count,
                    features,
                },
                battery,
            ))
        }

        fn read_receiver_pairing_device(
            &mut self,
            slot: u8,
            receiver_kind: Option<ReceiverKind>,
        ) -> io::Result<ReceiverPairingInfo> {
            match receiver_kind {
                Some(ReceiverKind::Bolt) => self.read_bolt_receiver_pairing_device(slot),
                Some(_) => self.read_unifying_receiver_pairing_device(slot),
                None => Ok(ReceiverPairingInfo::default()),
            }
        }

        fn read_unifying_receiver_pairing_device(
            &mut self,
            slot: u8,
        ) -> io::Result<ReceiverPairingInfo> {
            let mut info = self
                .read_register(
                    HIDPP_RECEIVER_DEVNUMBER,
                    HIDPP_REGISTER_RECEIVER_INFO,
                    &[
                        HIDPP_PAIRING_INFORMATION + slot.saturating_sub(1),
                        0x00,
                        0x00,
                    ],
                )?
                .map(|reply| unifying_pairing_info_from_register(&reply))
                .unwrap_or_default();

            if let Some(name) = self
                .read_register(
                    HIDPP_RECEIVER_DEVNUMBER,
                    HIDPP_REGISTER_RECEIVER_INFO,
                    &[HIDPP_DEVICE_NAME + slot.saturating_sub(1), 0x00, 0x00],
                )?
                .and_then(|reply| unifying_device_name_from_register(&reply))
            {
                info.name = Some(name);
            }

            if let Some(serial) = self
                .read_register(
                    HIDPP_RECEIVER_DEVNUMBER,
                    HIDPP_REGISTER_RECEIVER_INFO,
                    &[
                        HIDPP_EXTENDED_PAIRING_INFORMATION + slot.saturating_sub(1),
                        0x00,
                        0x00,
                    ],
                )?
                .and_then(|reply| reply.get(1..5).map(hex_upper))
                .filter(|value| !value.is_empty())
            {
                info.serial = Some(serial);
            }

            Ok(info)
        }

        fn read_bolt_receiver_pairing_device(
            &mut self,
            slot: u8,
        ) -> io::Result<ReceiverPairingInfo> {
            let mut info = self
                .read_register(
                    HIDPP_RECEIVER_DEVNUMBER,
                    HIDPP_REGISTER_RECEIVER_INFO,
                    &[HIDPP_BOLT_PAIRING_INFORMATION + slot, 0x00, 0x00],
                )?
                .map(|reply| bolt_pairing_info_from_register(&reply))
                .unwrap_or_default();

            if let Some(name) = self
                .read_register(
                    HIDPP_RECEIVER_DEVNUMBER,
                    HIDPP_REGISTER_RECEIVER_INFO,
                    &[HIDPP_BOLT_DEVICE_NAME + slot, 0x01, 0x00],
                )?
                .and_then(|reply| bolt_device_name_from_register(&reply))
            {
                info.name = Some(name);
            }

            Ok(info)
        }

        fn enumerate_features(
            &mut self,
            devnumber: u8,
            receiver_pairing: &ReceiverPairingInfo,
        ) -> io::Result<Vec<HidppFeatureInfo>> {
            if self.scan_depth == HidppScanDepth::DogiFeatures {
                return self.enumerate_dogi_features(devnumber, receiver_pairing);
            }

            let mut features = vec![HidppFeatureInfo {
                index: 0,
                feature_id: HIDPP_FEATURE_ROOT,
                name: hidpp_feature_name(HIDPP_FEATURE_ROOT),
                flags: 0,
                version: 0,
            }];
            let Some(feature_set_index) =
                self.feature_index(devnumber, HIDPP_FEATURE_FEATURE_SET)?
            else {
                return Ok(features);
            };

            let Some(count_reply) =
                self.feature_request(devnumber, feature_set_index, 0x00, &[])?
            else {
                return Ok(features);
            };
            let Some(raw_count) = count_reply.first().copied() else {
                return Ok(features);
            };
            let total_count = usize::from(raw_count)
                .saturating_add(1)
                .min(usize::from(u8::MAX) + 1);

            for index in 1..total_count {
                let index = index as u8;
                let Some(reply) =
                    self.feature_request(devnumber, feature_set_index, 0x10, &[index])?
                else {
                    continue;
                };
                if reply.len() < 2 {
                    continue;
                }

                let feature_id = u16::from_be_bytes([reply[0], reply[1]]);
                if features.iter().any(|feature| feature.index == index) {
                    continue;
                }

                features.push(HidppFeatureInfo {
                    index,
                    feature_id,
                    name: hidpp_feature_name(feature_id),
                    flags: reply.get(2).copied().unwrap_or_default(),
                    version: reply.get(3).copied().unwrap_or_default(),
                });
            }

            features.sort_by_key(|feature| feature.index);
            Ok(features)
        }

        fn enumerate_dogi_features(
            &mut self,
            devnumber: u8,
            receiver_pairing: &ReceiverPairingInfo,
        ) -> io::Result<Vec<HidppFeatureInfo>> {
            let mut features = vec![HidppFeatureInfo {
                index: 0,
                feature_id: HIDPP_FEATURE_ROOT,
                name: hidpp_feature_name(HIDPP_FEATURE_ROOT),
                flags: 0,
                version: 0,
            }];
            for &feature_id in HIDPP_DOGI_FEATURE_IDS {
                if feature_id == HIDPP_FEATURE_DEVICE_NAME
                    && receiver_pairing.name.is_some()
                    && receiver_pairing.kind.is_some()
                {
                    continue;
                }
                let Some(reply) = self.request(
                    devnumber,
                    HIDPP_FEATURE_ROOT,
                    &feature_id.to_be_bytes(),
                    HIDPP_REQUEST_TIMEOUT,
                )?
                else {
                    continue;
                };
                let Some(index) = reply.first().copied().filter(|index| *index != 0) else {
                    continue;
                };
                if features.iter().any(|feature| feature.index == index) {
                    continue;
                }
                features.push(HidppFeatureInfo {
                    index,
                    feature_id,
                    name: hidpp_feature_name(feature_id),
                    flags: reply.get(1).copied().unwrap_or_default(),
                    version: reply.get(2).copied().unwrap_or_default(),
                });
            }
            features.sort_by_key(|feature| feature.index);
            Ok(features)
        }

        fn read_device_name(
            &mut self,
            devnumber: u8,
            features: &[HidppFeatureInfo],
        ) -> io::Result<Option<String>> {
            let Some(index) = feature_index_in(features, HIDPP_FEATURE_DEVICE_NAME) else {
                return Ok(None);
            };
            let Some(length_reply) = self.feature_request(devnumber, index, 0x00, &[])? else {
                return Ok(None);
            };
            let Some(length) = length_reply.first().copied().map(usize::from) else {
                return Ok(None);
            };
            if length == 0 || length > 128 {
                return Ok(None);
            }

            let mut name = Vec::with_capacity(length);
            while name.len() < length {
                let offset = u8::try_from(name.len()).unwrap_or(u8::MAX);
                let Some(fragment) = self.feature_request(devnumber, index, 0x10, &[offset])?
                else {
                    return Ok(None);
                };
                if fragment.is_empty() {
                    return Ok(None);
                }

                let remaining = length - name.len();
                name.extend_from_slice(&fragment[..fragment.len().min(remaining)]);
            }

            Ok(clean_hidpp_string(&name))
        }

        fn read_device_kind(
            &mut self,
            devnumber: u8,
            features: &[HidppFeatureInfo],
        ) -> io::Result<Option<String>> {
            let Some(index) = feature_index_in(features, HIDPP_FEATURE_DEVICE_NAME) else {
                return Ok(None);
            };
            let Some(reply) = self.feature_request(devnumber, index, 0x20, &[])? else {
                return Ok(None);
            };

            Ok(reply.first().copied().and_then(hidpp_device_kind))
        }

        fn read_device_ids(
            &mut self,
            devnumber: u8,
            features: &[HidppFeatureInfo],
        ) -> io::Result<(Option<String>, Option<String>)> {
            let Some(index) = feature_index_in(features, HIDPP_FEATURE_DEVICE_FW_VERSION) else {
                return Ok((None, None));
            };
            let Some(reply) = self.feature_request(devnumber, index, 0x00, &[])? else {
                return Ok((None, None));
            };

            let unit_id = reply
                .get(1..5)
                .map(hex_upper)
                .filter(|value| !value.is_empty());
            let model_id = reply
                .get(7..13)
                .map(hex_upper)
                .filter(|value| !value.is_empty());

            Ok((unit_id, model_id))
        }

        fn read_battery_from_features(
            &mut self,
            devnumber: u8,
            features: &[HidppFeatureInfo],
        ) -> io::Result<Option<BatteryInfo>> {
            if let Some(index) = feature_index_in(features, HIDPP_FEATURE_UNIFIED_BATTERY)
                && let Some(reply) = self.feature_request(devnumber, index, 0x10, &[])?
                && let Some(battery) = decode_unified_battery(&reply)
            {
                return Ok(Some(battery));
            }

            if let Some(index) = feature_index_in(features, HIDPP_FEATURE_BATTERY_STATUS)
                && let Some(reply) = self.feature_request(devnumber, index, 0x00, &[])?
                && let Some(battery) = decode_battery_status(&reply)
            {
                return Ok(Some(battery));
            }

            if let Some(index) = feature_index_in(features, HIDPP_FEATURE_BATTERY_VOLTAGE)
                && let Some(reply) = self.feature_request(devnumber, index, 0x00, &[])?
                && let Some(battery) = decode_battery_voltage(&reply)
            {
                return Ok(Some(battery));
            }

            Ok(None)
        }

        fn feature_request(
            &mut self,
            devnumber: u8,
            feature_index: u8,
            function: u8,
            params: &[u8],
        ) -> io::Result<Option<Vec<u8>>> {
            self.request(
                devnumber,
                (u16::from(feature_index) << 8) | u16::from(function),
                params,
                HIDPP_REQUEST_TIMEOUT,
            )
        }

        fn feature_index(&mut self, devnumber: u8, feature: u16) -> io::Result<Option<u8>> {
            let params = feature.to_be_bytes();
            let Some(reply) = self.request(
                devnumber,
                HIDPP_FEATURE_ROOT,
                &params,
                HIDPP_REQUEST_TIMEOUT,
            )?
            else {
                return Ok(None);
            };

            Ok(reply.first().copied().filter(|index| *index != 0))
        }

        fn request(
            &mut self,
            devnumber: u8,
            request_id: u16,
            params: &[u8],
            timeout: Duration,
        ) -> io::Result<Option<Vec<u8>>> {
            let request_id = (request_id & 0xfff0) | u16::from(HIDPP_SW_ID);
            let mut request_data = Vec::with_capacity(2 + params.len());
            request_data.extend_from_slice(&request_id.to_be_bytes());
            request_data.extend_from_slice(params);

            self.request_raw(devnumber, &request_data, timeout)
        }

        fn read_register(
            &mut self,
            devnumber: u8,
            register: u16,
            params: &[u8],
        ) -> io::Result<Option<Vec<u8>>> {
            let request_id = 0x8100 | (register & 0x02ff);
            let mut request_data = Vec::with_capacity(2 + params.len());
            request_data.extend_from_slice(&request_id.to_be_bytes());
            request_data.extend_from_slice(params);

            self.request_raw(devnumber, &request_data, HIDPP_REQUEST_TIMEOUT)
        }

        fn request_raw(
            &mut self,
            devnumber: u8,
            request_data: &[u8],
            timeout: Duration,
        ) -> io::Result<Option<Vec<u8>>> {
            let report = build_hidpp_report(devnumber, request_data)?;
            self.drain_input()?;
            self.write_report(&report, timeout)?;

            let deadline = Instant::now() + timeout;
            loop {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    return Ok(None);
                };
                let Some(reply) = self.read_report(remaining)? else {
                    return Ok(None);
                };

                if reply.len() < 4 {
                    continue;
                }

                let report_id = reply[0];
                if !matches!(report_id, HIDPP_SHORT_REPORT_ID | HIDPP_LONG_REPORT_ID) {
                    continue;
                }

                let reply_devnumber = reply[1];
                if reply_devnumber != devnumber && reply_devnumber != (devnumber ^ 0xff) {
                    continue;
                }

                let reply_data = &reply[2..];
                if is_hidpp_error_reply(report_id, reply_data, request_data) {
                    return Ok(None);
                }

                if reply_data.len() >= 2 && reply_data[..2] == request_data[..2] {
                    return Ok(Some(reply_data[2..].to_vec()));
                }
            }
        }

        fn write_report(&mut self, report: &[u8], timeout: Duration) -> io::Result<()> {
            let deadline = Instant::now() + timeout;
            loop {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting to write HID++ report",
                    ));
                };

                if !wait_fd(self.file.as_raw_fd(), libc::POLLOUT, remaining)? {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting to write HID++ report",
                    ));
                }

                match self.file.write(report) {
                    Ok(written) if written == report.len() => return Ok(()),
                    Ok(written) => {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            format!("short HID++ report write: {written}/{}", report.len()),
                        ));
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
        }

        fn read_report(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
            if !wait_fd(self.file.as_raw_fd(), libc::POLLIN, timeout)? {
                return Ok(None);
            }

            let mut buffer = [0_u8; HIDPP_MAX_READ_LEN];
            loop {
                match self.file.read(&mut buffer) {
                    Ok(0) => return Ok(None),
                    Ok(read) => return Ok(Some(buffer[..read].to_vec())),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
        }

        fn drain_input(&mut self) -> io::Result<()> {
            let mut buffer = [0_u8; HIDPP_MAX_READ_LEN];
            loop {
                match self.file.read(&mut buffer) {
                    Ok(0) => return Ok(()),
                    Ok(_) => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
        }
    }

    impl HidppProbeClient for HidppClient {
        fn read_receiver_pairing_device(
            &mut self,
            slot: u8,
            receiver_kind: Option<ReceiverKind>,
        ) -> io::Result<ReceiverPairingInfo> {
            HidppClient::read_receiver_pairing_device(self, slot, receiver_kind)
        }

        fn ping(&mut self, devnumber: u8) -> io::Result<Option<HidppProtocolVersion>> {
            HidppClient::ping(self, devnumber)
        }

        fn read_paired_device(
            &mut self,
            devnumber: u8,
            protocol: HidppProtocolVersion,
            receiver_pairing: &ReceiverPairingInfo,
        ) -> io::Result<(PairedDeviceInfo, Option<BatteryInfo>)> {
            HidppClient::read_paired_device(self, devnumber, protocol, receiver_pairing)
        }

        fn read_battery(
            &mut self,
            devnumber: u8,
            features: &[HidppFeatureInfo],
        ) -> io::Result<Option<BatteryInfo>> {
            self.read_battery_from_features(devnumber, features)
        }
    }

    fn build_hidpp_report(devnumber: u8, request_data: &[u8]) -> io::Result<Vec<u8>> {
        let report_len = if request_data.len() > HIDPP_SHORT_REPORT_LEN - 2 {
            HIDPP_LONG_REPORT_LEN
        } else {
            HIDPP_SHORT_REPORT_LEN
        };

        if request_data.len() > report_len - 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "HID++ request payload is too large",
            ));
        }

        let mut report = vec![0_u8; report_len];
        report[0] = if report_len == HIDPP_LONG_REPORT_LEN {
            HIDPP_LONG_REPORT_ID
        } else {
            HIDPP_SHORT_REPORT_ID
        };
        report[1] = devnumber;
        report[2..2 + request_data.len()].copy_from_slice(request_data);
        Ok(report)
    }

    fn is_hidpp_error_reply(report_id: u8, reply_data: &[u8], request_data: &[u8]) -> bool {
        if reply_data.len() < 4 {
            return false;
        }

        let matches_request = reply_data[1..3] == request_data[..2];
        (report_id == HIDPP_SHORT_REPORT_ID && reply_data[0] == 0x8f && matches_request)
            || (reply_data[0] == 0xff && matches_request)
    }

    fn decode_unified_battery(report: &[u8]) -> Option<BatteryInfo> {
        let discharge = *report.first()?;
        let level = report.get(1).copied().unwrap_or_default();
        let status_byte = report.get(2).copied().unwrap_or_default();
        let level_percent = if discharge != 0 {
            Some(discharge.min(100))
        } else {
            unified_battery_level(level)
        };
        let status = normalize_battery_status(hidpp_battery_status(status_byte), level_percent);

        Some(BatteryInfo {
            level_percent,
            status,
            source: BatterySource::Hidpp,
            detail: Some("HID++ unified battery".to_owned()),
        })
    }

    fn decode_battery_status(report: &[u8]) -> Option<BatteryInfo> {
        let discharge = *report.first()?;
        let next_threshold = report.get(1).copied().unwrap_or_default();
        let status_byte = report.get(2).copied().unwrap_or_default();
        let level_percent = (discharge != 0).then_some(discharge.min(100));
        let status = normalize_battery_status(hidpp_battery_status(status_byte), level_percent);
        let detail = if next_threshold == 0 {
            "HID++ battery status".to_owned()
        } else {
            format!("HID++ battery status, next threshold {next_threshold}%")
        };

        Some(BatteryInfo {
            level_percent,
            status,
            source: BatterySource::Hidpp,
            detail: Some(detail),
        })
    }

    fn decode_battery_voltage(report: &[u8]) -> Option<BatteryInfo> {
        let voltage = u16::from_be_bytes([*report.first()?, *report.get(1)?]);
        let flags = report.get(2).copied().unwrap_or_default();
        let level_percent = estimate_lithium_voltage_percent(voltage);
        let charging = flags & 0x80 != 0;
        let status = if charging {
            if flags & 0x03 == 0x03 {
                BatteryStatus::Full
            } else {
                BatteryStatus::Charging
            }
        } else {
            normalize_battery_status(BatteryStatus::Discharging, level_percent)
        };

        Some(BatteryInfo {
            level_percent,
            status,
            source: BatterySource::Hidpp,
            detail: Some(format!("HID++ battery voltage {voltage} mV")),
        })
    }

    fn unified_battery_level(level: u8) -> Option<u8> {
        match level {
            8 => Some(90),
            4 => Some(50),
            2 => Some(20),
            1 => Some(5),
            0 => Some(0),
            _ => None,
        }
    }

    fn hidpp_battery_status(value: u8) -> BatteryStatus {
        match value {
            0x00 => BatteryStatus::Discharging,
            0x01 | 0x02 | 0x04 => BatteryStatus::Charging,
            0x03 => BatteryStatus::Full,
            0x05 => BatteryStatus::Offline,
            0x06 => BatteryStatus::Unknown,
            _ => BatteryStatus::Unknown,
        }
    }

    fn normalize_battery_status(status: BatteryStatus, level_percent: Option<u8>) -> BatteryStatus {
        match (status, level_percent) {
            (BatteryStatus::Discharging, Some(level)) if level <= 5 => BatteryStatus::Critical,
            (BatteryStatus::Discharging, Some(level)) if level <= 20 => BatteryStatus::Low,
            _ => status,
        }
    }

    fn estimate_lithium_voltage_percent(voltage: u16) -> Option<u8> {
        if !(3_000..=4_500).contains(&voltage) {
            return None;
        }

        let percent = voltage.saturating_sub(3_300).saturating_mul(100) / 900;
        Some(percent.min(100) as u8)
    }

    fn set_nonblocking(file: &File) -> io::Result<()> {
        let fd = file.as_raw_fd();
        // SAFETY: fcntl is called with a valid file descriptor owned by File.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: fcntl does not take ownership of fd and only changes descriptor flags.
        let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    fn wait_fd(fd: i32, events: libc::c_short, timeout: Duration) -> io::Result<bool> {
        let mut pollfd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;

        loop {
            // SAFETY: pollfd points to a valid pollfd for the duration of the call.
            let result = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
            if result > 0 {
                if pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    return Err(io::Error::other("hidraw descriptor is no longer usable"));
                }
                return Ok(pollfd.revents & events != 0);
            }
            if result == 0 {
                return Ok(false);
            }

            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    fn parse_hid_id(value: Option<&str>) -> Option<(u16, u16, u16)> {
        let mut parts = value?.split(':');
        let bus = parse_hex_u16(parts.next()?)?;
        let vendor = parse_hex_u32_low_u16(parts.next()?)?;
        let product = parse_hex_u32_low_u16(parts.next()?)?;
        Some((bus, vendor, product))
    }

    fn parse_hex_u16(value: &str) -> Option<u16> {
        u16::from_str_radix(value.trim().trim_start_matches("0x"), 16).ok()
    }

    fn parse_hex_i32(value: &str) -> Option<i32> {
        parse_hex_u16(value).map(i32::from)
    }

    fn parse_hex_u32_low_u16(value: &str) -> Option<u16> {
        let parsed = u32::from_str_radix(value.trim().trim_start_matches("0x"), 16).ok()?;
        Some((parsed & 0xffff) as u16)
    }

    fn parse_interface_number_from_modalias(value: Option<&String>) -> Option<i32> {
        let modalias = value?;
        let index = modalias.find("in")?;
        let number = modalias.get(index + 2..index + 4)?;
        parse_hex_i32(number)
    }

    fn clean_optional_string(value: Option<&str>) -> Option<String> {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    }

    fn first_non_empty(values: &[Option<String>]) -> Option<String> {
        values
            .iter()
            .flatten()
            .find(|value| !value.is_empty())
            .cloned()
    }

    fn summarize_report_descriptor(descriptor: &[u8]) -> ReportDescriptorInfo {
        let mut info = ReportDescriptorInfo {
            byte_len: descriptor.len(),
            ..ReportDescriptorInfo::default()
        };
        let mut index = 0;
        let mut usage_page = None;
        let mut report_id = None;

        while index < descriptor.len() {
            let prefix = descriptor[index];
            index += 1;

            if prefix == 0xfe {
                let Some(size) = descriptor.get(index).copied() else {
                    break;
                };
                let Some(next_index) = index.checked_add(2 + usize::from(size)) else {
                    break;
                };
                index = next_index.min(descriptor.len());
                continue;
            }

            let size = match prefix & 0b11 {
                0 => 0,
                1 => 1,
                2 => 2,
                _ => 4,
            };
            let item_type = (prefix >> 2) & 0b11;
            let tag = (prefix >> 4) & 0b1111;
            let Some(data) = descriptor.get(index..index + size) else {
                break;
            };
            index += size;

            if item_type == 1 && tag == 0 {
                let page = read_le_u32(data) as u16;
                usage_page = Some(page);
                if page >= HIDPP_USAGE_PAGE && !info.vendor_usage_pages.contains(&page) {
                    info.vendor_usage_pages.push(page);
                }
            } else if item_type == 1 && tag == 8 {
                let id = read_le_u32(data) as u8;
                report_id = (id != 0).then_some(id);
                if id != 0 && !info.report_ids.contains(&id) {
                    info.report_ids.push(id);
                }
            } else if item_type == 2 && tag == 0 {
                let usage = read_le_u32(data) as u16;
                if usage_page == Some(HIDPP_USAGE_PAGE) && matches!(usage, 0x0001 | 0x0002) {
                    info.hidpp_usage = Some(HidUsage {
                        usage_page: HIDPP_USAGE_PAGE,
                        usage,
                    });
                }
            } else if item_type == 0 {
                match tag {
                    8 => info.has_input_reports = true,
                    9 => info.has_output_reports = true,
                    11 => info.has_feature_reports = true,
                    _ => {}
                }

                if let Some(id) = report_id
                    && !info.report_ids.contains(&id)
                {
                    info.report_ids.push(id);
                }
            }
        }

        info.report_ids.sort_unstable();
        info.vendor_usage_pages.sort_unstable();
        info
    }

    fn read_le_u32(data: &[u8]) -> u32 {
        data.iter().enumerate().fold(0, |value, (index, byte)| {
            value | (u32::from(*byte) << (index * 8))
        })
    }

    #[allow(dead_code)]
    fn canonicalize_lossy(path: PathBuf) -> String {
        path.canonicalize()
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    #[cfg(test)]
    mod tests {
        use std::collections::VecDeque;

        use super::*;

        struct FakeProbeClient {
            pairing: ReceiverPairingInfo,
            pings: VecDeque<io::Result<Option<HidppProtocolVersion>>>,
            paired_devices: VecDeque<io::Result<(PairedDeviceInfo, Option<BatteryInfo>)>>,
            batteries: VecDeque<io::Result<Option<BatteryInfo>>>,
            ping_calls: usize,
            paired_device_calls: usize,
            battery_calls: usize,
        }

        impl FakeProbeClient {
            fn new(pairing: ReceiverPairingInfo) -> Self {
                Self {
                    pairing,
                    pings: VecDeque::new(),
                    paired_devices: VecDeque::new(),
                    batteries: VecDeque::new(),
                    ping_calls: 0,
                    paired_device_calls: 0,
                    battery_calls: 0,
                }
            }
        }

        impl HidppProbeClient for FakeProbeClient {
            fn read_receiver_pairing_device(
                &mut self,
                slot: u8,
                _receiver_kind: Option<ReceiverKind>,
            ) -> io::Result<ReceiverPairingInfo> {
                Ok(if slot == 1 {
                    self.pairing.clone()
                } else {
                    ReceiverPairingInfo {
                        slot_state_known: self.pairing.slot_state_known,
                        ..ReceiverPairingInfo::default()
                    }
                })
            }

            fn ping(&mut self, _devnumber: u8) -> io::Result<Option<HidppProtocolVersion>> {
                self.ping_calls += 1;
                self.pings.pop_front().unwrap_or(Ok(None))
            }

            fn read_paired_device(
                &mut self,
                _devnumber: u8,
                _protocol: HidppProtocolVersion,
                _receiver_pairing: &ReceiverPairingInfo,
            ) -> io::Result<(PairedDeviceInfo, Option<BatteryInfo>)> {
                self.paired_device_calls += 1;
                self.paired_devices.pop_front().unwrap_or_else(|| {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "fake paired-device response queue is empty",
                    ))
                })
            }

            fn read_battery(
                &mut self,
                _devnumber: u8,
                _features: &[HidppFeatureInfo],
            ) -> io::Result<Option<BatteryInfo>> {
                self.battery_calls += 1;
                self.batteries.pop_front().unwrap_or(Ok(None))
            }
        }

        fn test_protocol() -> HidppProtocolVersion {
            HidppProtocolVersion { major: 4, minor: 5 }
        }

        fn test_paired_device(feature_count: usize) -> PairedDeviceInfo {
            PairedDeviceInfo {
                slot: 1,
                name: Some("MX Master 3S".to_owned()),
                kind: Some("mouse".to_owned()),
                wpid: None,
                protocol: Some(test_protocol()),
                unit_id: Some("AABBCCDD".to_owned()),
                model_id: Some("B03400000000".to_owned()),
                feature_count,
                features: Vec::new(),
            }
        }

        fn test_receiver_pairing() -> ReceiverPairingInfo {
            ReceiverPairingInfo {
                name: Some("MX Master 3S".to_owned()),
                kind: Some("mouse".to_owned()),
                wpid: Some("B034".to_owned()),
                serial: Some("AABBCCDD".to_owned()),
                slot_state_known: true,
            }
        }

        #[test]
        fn retries_a_known_receiver_slot_until_the_device_answers() {
            let mut client = FakeProbeClient::new(test_receiver_pairing());
            client.pings.push_back(Ok(None));
            client.pings.push_back(Ok(Some(test_protocol())));
            client.paired_devices.push_back(Ok((
                test_paired_device(24),
                decode_unified_battery(&[85, 4, 0]),
            )));
            let mut pauses = Vec::new();

            let probe = probe_hidpp_device(&mut client, Some(ReceiverKind::Bolt), |delay| {
                pauses.push(delay)
            });

            assert_eq!(client.ping_calls, 2);
            assert_eq!(client.paired_device_calls, 1);
            assert_eq!(pauses, vec![HIDPP_PROBE_RETRY_DELAYS[0]]);
            assert_eq!(
                probe.battery.and_then(|battery| battery.level_percent),
                Some(85)
            );
            assert_eq!(
                probe
                    .paired_device
                    .as_ref()
                    .and_then(|device| device.wpid.as_deref()),
                Some("B034")
            );
        }

        #[test]
        fn retries_a_transient_ping_io_error() {
            let mut client = FakeProbeClient::new(test_receiver_pairing());
            client.pings.push_back(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "transient receiver timeout",
            )));
            client.pings.push_back(Ok(Some(test_protocol())));
            client.paired_devices.push_back(Ok((
                test_paired_device(24),
                decode_unified_battery(&[75, 4, 0]),
            )));
            let mut pauses = Vec::new();

            let probe = probe_hidpp_device(&mut client, Some(ReceiverKind::Bolt), |delay| {
                pauses.push(delay)
            });

            assert_eq!(client.ping_calls, 2);
            assert_eq!(pauses, vec![HIDPP_PROBE_RETRY_DELAYS[0]]);
            assert_eq!(
                probe.battery.and_then(|battery| battery.level_percent),
                Some(75)
            );
        }

        #[test]
        fn retries_an_incomplete_feature_probe_until_battery_is_available() {
            let mut client = FakeProbeClient::new(test_receiver_pairing());
            client.pings.push_back(Ok(Some(test_protocol())));
            client
                .paired_devices
                .push_back(Ok((test_paired_device(8), None)));
            client.paired_devices.push_back(Ok((
                test_paired_device(24),
                decode_unified_battery(&[70, 4, 0]),
            )));
            let mut pauses = Vec::new();

            let probe = probe_hidpp_device(&mut client, Some(ReceiverKind::Bolt), |delay| {
                pauses.push(delay)
            });

            assert_eq!(client.ping_calls, 1);
            assert_eq!(client.paired_device_calls, 2);
            assert_eq!(pauses, vec![HIDPP_PROBE_RETRY_DELAYS[0]]);
            assert_eq!(
                probe.battery.and_then(|battery| battery.level_percent),
                Some(70)
            );
        }

        #[test]
        fn battery_retry_reuses_the_static_feature_map() {
            let mut client = FakeProbeClient::new(test_receiver_pairing());
            client.pings.push_back(Ok(Some(test_protocol())));
            let mut paired_device = test_paired_device(24);
            paired_device.features.push(HidppFeatureInfo {
                index: 9,
                feature_id: HIDPP_FEATURE_UNIFIED_BATTERY,
                name: "UNIFIED_BATTERY".to_owned(),
                flags: 0,
                version: 1,
            });
            client.paired_devices.push_back(Ok((paired_device, None)));
            client
                .batteries
                .push_back(Ok(decode_unified_battery(&[68, 4, 0])));
            let mut pauses = Vec::new();

            let probe = probe_hidpp_device(&mut client, Some(ReceiverKind::Bolt), |delay| {
                pauses.push(delay)
            });

            assert_eq!(client.paired_device_calls, 1);
            assert_eq!(client.battery_calls, 1);
            assert_eq!(pauses, vec![HIDPP_PROBE_RETRY_DELAYS[0]]);
            assert_eq!(
                probe.battery.and_then(|battery| battery.level_percent),
                Some(68)
            );
        }

        #[test]
        fn stops_after_the_retry_budget_and_keeps_receiver_identity() {
            let mut client = FakeProbeClient::new(test_receiver_pairing());
            client.pings.extend([Ok(None), Ok(None), Ok(None)]);
            let mut pauses = Vec::new();

            let probe = probe_hidpp_device(&mut client, Some(ReceiverKind::Bolt), |delay| {
                pauses.push(delay)
            });

            assert_eq!(client.ping_calls, 3);
            assert_eq!(client.paired_device_calls, 0);
            assert_eq!(pauses, HIDPP_PROBE_RETRY_DELAYS);
            assert_eq!(
                probe
                    .paired_device
                    .as_ref()
                    .and_then(|device| device.wpid.as_deref()),
                Some("B034")
            );
            assert!(probe.battery.is_none());
            assert!(
                probe
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains("after 3 attempts"))
            );
        }

        #[test]
        fn skips_receiver_slots_confirmed_as_empty() {
            let mut client = FakeProbeClient::new(ReceiverPairingInfo {
                slot_state_known: true,
                ..ReceiverPairingInfo::default()
            });
            let mut pauses = Vec::new();

            let probe = probe_hidpp_device(&mut client, Some(ReceiverKind::Bolt), |delay| {
                pauses.push(delay)
            });

            assert_eq!(client.ping_calls, 0);
            assert!(pauses.is_empty());
            assert!(probe.paired_device.is_none());
        }

        #[test]
        fn ui_probe_requests_only_dogi_relevant_features() {
            let unique = HIDPP_DOGI_FEATURE_IDS
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>();

            assert_eq!(unique.len(), HIDPP_DOGI_FEATURE_IDS.len());
            for required in [
                HIDPP_FEATURE_DEVICE_FW_VERSION,
                HIDPP_FEATURE_DEVICE_NAME,
                HIDPP_FEATURE_UNIFIED_BATTERY,
                HIDPP_FEATURE_REPROG_CONTROLS_V4,
                HIDPP_FEATURE_SMART_SHIFT_ENHANCED,
                HIDPP_FEATURE_HIRES_WHEEL,
                HIDPP_FEATURE_THUMB_WHEEL,
                HIDPP_FEATURE_POINTER_SPEED,
            ] {
                assert!(unique.contains(&required));
            }
        }

        #[test]
        fn falls_back_to_one_ping_when_receiver_slot_state_is_unknown() {
            let mut client = FakeProbeClient::new(ReceiverPairingInfo::default());
            client.pings.extend((0..7).map(|_| Ok(None)));
            let mut pauses = Vec::new();

            let probe = probe_hidpp_device(&mut client, Some(ReceiverKind::Bolt), |delay| {
                pauses.push(delay)
            });

            assert_eq!(client.ping_calls, 7);
            assert!(pauses.is_empty());
            assert!(probe.paired_device.is_none());
        }

        #[test]
        fn parses_hid_id() {
            assert_eq!(
                parse_hid_id(Some("0003:0000046D:0000C548")),
                Some((0x0003, 0x046d, 0xc548))
            );
        }

        #[test]
        fn detects_hidpp_descriptor_usage() {
            let descriptor = [
                0x06, 0x00, 0xff, // Usage Page (vendor 0xff00)
                0x09, 0x02, // Usage 2
                0xa1, 0x01, // Collection
            ];

            let summary = summarize_report_descriptor(&descriptor);

            assert_eq!(
                summary.hidpp_usage,
                Some(HidUsage {
                    usage_page: 0xff00,
                    usage: 0x0002,
                })
            );
            assert!(summary.is_hidpp_interface());
        }

        #[test]
        fn summarizes_report_ids_and_report_kinds() {
            let descriptor = [
                0x85, 0x10, // Report ID 16
                0x81, 0x00, // Input
                0x91, 0x00, // Output
                0xb1, 0x00, // Feature
            ];

            let summary = summarize_report_descriptor(&descriptor);

            assert_eq!(summary.report_ids, vec![0x10]);
            assert!(summary.has_input_reports);
            assert!(summary.has_output_reports);
            assert!(summary.has_feature_reports);
        }

        #[test]
        fn builds_hidpp_short_report() {
            let report = build_hidpp_report(1, &[0x00, 0x18, 0x00, 0x00, 0xa4]).unwrap();

            assert_eq!(report, vec![0x10, 0x01, 0x00, 0x18, 0x00, 0x00, 0xa4]);
        }

        #[test]
        fn builds_hidpp_long_report() {
            let report = build_hidpp_report(0xff, &[0x02, 0x18, 1, 2, 3, 4]).unwrap();

            assert_eq!(report[0], 0x11);
            assert_eq!(report[1], 0xff);
            assert_eq!(&report[2..8], &[0x02, 0x18, 1, 2, 3, 4]);
            assert_eq!(report.len(), HIDPP_LONG_REPORT_LEN);
        }

        #[test]
        fn decodes_unified_battery_percent() {
            let battery = decode_unified_battery(&[85, 4, 0x00, 0x00]).unwrap();

            assert_eq!(battery.source, BatterySource::Hidpp);
            assert_eq!(battery.level_percent, Some(85));
            assert_eq!(battery.status, BatteryStatus::Discharging);
        }

        #[test]
        fn decodes_unified_battery_approximation() {
            let battery = decode_unified_battery(&[0, 2, 0x00, 0x00]).unwrap();

            assert_eq!(battery.level_percent, Some(20));
            assert_eq!(battery.status, BatteryStatus::Low);
        }

        #[test]
        fn decodes_battery_status() {
            let battery = decode_battery_status(&[5, 10, 0x00]).unwrap();

            assert_eq!(battery.level_percent, Some(5));
            assert_eq!(battery.status, BatteryStatus::Critical);
            assert!(
                battery
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains("next threshold 10%"))
            );
        }

        #[test]
        fn maps_hidpp_feature_names() {
            assert_eq!(
                hidpp_feature_name(HIDPP_FEATURE_POINTER_SPEED),
                "POINTER_SPEED"
            );
            assert_eq!(hidpp_feature_name(0xbeef), "UNKNOWN_BEEF");
        }

        #[test]
        fn decodes_unifying_receiver_pairing_info() {
            let reply = [0x20, 0x00, 0x08, 0xb0, 0x34, 0x00, 0x00, 0x04];
            let info = unifying_pairing_info_from_register(&reply);

            assert_eq!(info.wpid.as_deref(), Some("B034"));
            assert_eq!(info.kind.as_deref(), Some("mouse"));
        }

        #[test]
        fn decodes_bolt_receiver_pairing_info() {
            let reply = [0x51, 0x04, 0x34, 0xb0, 0xaa, 0xbb, 0xcc, 0xdd];
            let info = bolt_pairing_info_from_register(&reply);

            assert_eq!(info.wpid.as_deref(), Some("B034"));
            assert_eq!(info.kind.as_deref(), Some("mouse"));
            assert_eq!(info.serial.as_deref(), Some("AABBCCDD"));
        }

        #[test]
        fn decodes_receiver_device_names() {
            let unifying = [
                0x40, 12, b'M', b'X', b' ', b'M', b'a', b's', b't', b'e', b'r', b' ', b'3', b'S',
            ];
            let bolt = [
                0x61, 0x01, 12, b'M', b'X', b' ', b'M', b'a', b's', b't', b'e', b'r', b' ', b'3',
                b'S',
            ];

            assert_eq!(
                unifying_device_name_from_register(&unifying).as_deref(),
                Some("MX Master 3S")
            );
            assert_eq!(
                bolt_device_name_from_register(&bolt).as_deref(),
                Some("MX Master 3S")
            );
        }

        #[test]
        fn infers_capabilities_from_features() {
            let features = vec![
                HidppFeatureInfo {
                    index: 5,
                    feature_id: HIDPP_FEATURE_POINTER_SPEED,
                    name: hidpp_feature_name(HIDPP_FEATURE_POINTER_SPEED),
                    flags: 0,
                    version: 1,
                },
                HidppFeatureInfo {
                    index: 6,
                    feature_id: HIDPP_FEATURE_HIRES_WHEEL,
                    name: hidpp_feature_name(HIDPP_FEATURE_HIRES_WHEEL),
                    flags: 0,
                    version: 1,
                },
                HidppFeatureInfo {
                    index: 7,
                    feature_id: HIDPP_FEATURE_REPROG_CONTROLS_V4,
                    name: hidpp_feature_name(HIDPP_FEATURE_REPROG_CONTROLS_V4),
                    flags: 0,
                    version: 4,
                },
            ];
            let mut capabilities = DeviceCapabilities::default();

            apply_hidpp_feature_capabilities(&mut capabilities, &features);

            assert_eq!(capabilities.dpi, CapabilityState::Supported);
            assert_eq!(capabilities.wheel_mode, CapabilityState::Supported);
            assert_eq!(capabilities.button_mapping, CapabilityState::Supported);
        }

        #[test]
        fn encodes_pointer_speed_as_hidpp_multiplier() {
            assert_eq!(pointer_speed_hidpp_value(50), 128);
            assert_eq!(pointer_speed_hidpp_value(100), 256);
            assert_eq!(pointer_speed_hidpp_value(200), 511);
        }

        #[test]
        fn encodes_smart_shift_payloads() {
            let smart = Master3sSettings {
                smart_shift_enabled: true,
                smart_shift_threshold: 45,
                ratchet_mode: WheelRatchetMode::SmartShift,
                ..Master3sSettings::default()
            };
            let ratchet = Master3sSettings {
                smart_shift_enabled: false,
                ratchet_mode: WheelRatchetMode::SmartShift,
                ..Master3sSettings::default()
            };
            let free_spin = Master3sSettings {
                smart_shift_enabled: true,
                ratchet_mode: WheelRatchetMode::FreeSpin,
                ..Master3sSettings::default()
            };

            assert_eq!(smart_shift_payload(&smart), vec![0, 45]);
            assert_eq!(smart_shift_payload(&ratchet), vec![2]);
            assert_eq!(smart_shift_payload(&free_spin), vec![1]);
        }

        #[test]
        fn merges_hires_wheel_flags_without_touching_diversion() {
            assert_eq!(merge_hires_wheel_flags(0x01, Some(true), Some(false)), 0x03);
            assert_eq!(merge_hires_wheel_flags(0x07, Some(false), Some(true)), 0x05);
        }

        #[test]
        fn merges_only_the_explicitly_changed_hires_wheel_flag() {
            assert_eq!(merge_hires_wheel_flags(0x05, Some(true), None), 0x07);
            assert_eq!(merge_hires_wheel_flags(0x03, None, Some(true)), 0x07);
            assert_eq!(merge_hires_wheel_flags(0x07, Some(false), None), 0x05);
            assert_eq!(merge_hires_wheel_flags(0x07, None, Some(false)), 0x03);
        }

        #[test]
        fn encodes_thumb_wheel_diversion_without_touching_direction() {
            assert!(!thumb_wheel_diversion_target(
                ThumbWheelMode::HorizontalScroll,
                dogi_core::DEFAULT_THUMB_WHEEL_SPEED_PERCENT,
            ));
            assert!(thumb_wheel_diversion_target(
                ThumbWheelMode::HorizontalScroll,
                140,
            ));
            assert!(thumb_wheel_diversion_target(
                ThumbWheelMode::Disabled,
                dogi_core::DEFAULT_THUMB_WHEEL_SPEED_PERCENT,
            ));
            assert!(thumb_wheel_diversion_target(
                ThumbWheelMode::Zoom,
                dogi_core::DEFAULT_THUMB_WHEEL_SPEED_PERCENT,
            ));
            assert_eq!(
                merge_thumb_wheel_diversion(&[0x02, 0x01], true),
                Some(vec![0x03, 0x01])
            );
            assert_eq!(
                merge_thumb_wheel_diversion(&[0x03, 0x01], false),
                Some(vec![0x02, 0x01])
            );
            assert_eq!(merge_thumb_wheel_diversion(&[0x00], true), None);
        }

        #[test]
        fn decodes_thumb_wheel_diverted_resolution_and_direction() {
            assert_eq!(
                decode_thumb_wheel_runtime_info(&[0x00, 0x0f, 0x00, 0x08, 0x01, 0x0f, 0, 5]),
                Some(ThumbWheelRuntimeInfo {
                    resolution: 8,
                    direction: 1,
                })
            );
            assert_eq!(
                decode_thumb_wheel_runtime_info(&[0x00, 0x0f, 0x00, 0x00, 0x00]),
                Some(ThumbWheelRuntimeInfo {
                    resolution: 1,
                    direction: -1,
                })
            );
            assert_eq!(decode_thumb_wheel_runtime_info(&[0x00, 0x0f]), None);
        }

        #[test]
        fn parses_reprogrammable_control_v4_info() {
            let info =
                parse_reprog_control_info(&[0x00, 0x53, 0x00, 0x3c, 0x71, 0x00, 0x02, 0x03, 0x01])
                    .unwrap();

            assert_eq!(
                info,
                ReprogControlInfo {
                    control_id: 0x0053,
                    flags: 0x0171,
                }
            );
            assert!(info.flags & REPROG_KEY_FLAG_DIVERTABLE != 0);
            assert!(info.flags & REPROG_KEY_FLAG_VIRTUAL == 0);
        }

        #[test]
        fn encodes_reprogrammable_control_diversion_payloads() {
            assert_eq!(
                reprogrammable_control_diversion_payload(0x0053, true, false),
                [0x00, 0x53, 0x23, 0x00, 0x00]
            );
            assert_eq!(
                reprogrammable_control_diversion_payload(0x0053, false, false),
                [0x00, 0x53, 0x22, 0x00, 0x00]
            );
            assert_eq!(
                reprogrammable_control_diversion_payload(0x00c3, true, true),
                [0x00, 0xc3, 0x33, 0x00, 0x00]
            );
        }

        #[test]
        fn cleans_hidpp_strings() {
            assert_eq!(
                clean_hidpp_string(b"MX Master 3S\0\0"),
                Some("MX Master 3S".to_owned())
            );
            assert_eq!(clean_hidpp_string(b"\0\0"), None);
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use std::time::Duration;

    use dogi_core::{
        DeviceInfo, DogiError, Master3sSettings, Result, SettingsApplyPlan, SettingsApplyReport,
    };

    use crate::Master3sRuntimeEvent;

    pub fn scan_devices() -> Result<Vec<DeviceInfo>> {
        Err(DogiError::BackendUnavailable(
            "only Linux hidraw/sysfs scanning is implemented".to_owned(),
        ))
    }

    pub fn scan_device_inventory() -> Result<Vec<DeviceInfo>> {
        scan_devices()
    }

    pub fn scan_devices_for_ui() -> Result<Vec<DeviceInfo>> {
        scan_devices()
    }

    pub fn scan_all_devices() -> Result<Vec<DeviceInfo>> {
        scan_devices()
    }

    pub fn find_device(_id: &str) -> Result<DeviceInfo> {
        Err(DogiError::BackendUnavailable(
            "only Linux hidraw/sysfs scanning is implemented".to_owned(),
        ))
    }

    pub fn apply_master3s_settings_plan(
        _device_id: &str,
        _settings: &Master3sSettings,
        _plan: &SettingsApplyPlan,
    ) -> Result<SettingsApplyReport> {
        Err(DogiError::BackendUnavailable(
            "only Linux hidraw HID++ apply is implemented".to_owned(),
        ))
    }

    pub fn listen_master3s_runtime_events(
        _device_id: &str,
        _event_limit: usize,
        _idle_timeout: Duration,
    ) -> Result<Vec<Master3sRuntimeEvent>> {
        Err(DogiError::BackendUnavailable(
            "only Linux hidraw HID++ runtime listening is implemented".to_owned(),
        ))
    }

    pub struct Master3sRuntimeEventListener;

    impl Master3sRuntimeEventListener {
        pub fn open(_device_id: &str) -> Result<Self> {
            Err(DogiError::BackendUnavailable(
                "only Linux hidraw HID++ runtime listening is implemented".to_owned(),
            ))
        }

        pub fn read_events(
            &mut self,
            _event_limit: usize,
            _idle_timeout: Duration,
        ) -> Result<Vec<Master3sRuntimeEvent>> {
            Err(DogiError::BackendUnavailable(
                "only Linux hidraw HID++ runtime listening is implemented".to_owned(),
            ))
        }
    }
}
