use std::fmt;

use serde::{Deserialize, Serialize};

pub const LOGITECH_VENDOR_ID: u16 = 0x046d;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusKind {
    Usb,
    Bluetooth,
    I2c,
    Spi,
    Virtual,
    Unknown,
}

impl fmt::Display for BusKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Usb => "usb",
            Self::Bluetooth => "bluetooth",
            Self::I2c => "i2c",
            Self::Spi => "spi",
            Self::Virtual => "virtual",
            Self::Unknown => "unknown",
        };
        f.write_str(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionKind {
    Usb,
    Bluetooth,
    Unifying,
    Bolt,
    Lightspeed,
    Unknown,
}

impl fmt::Display for ConnectionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Usb => "usb",
            Self::Bluetooth => "bluetooth",
            Self::Unifying => "unifying",
            Self::Bolt => "bolt",
            Self::Lightspeed => "lightspeed",
            Self::Unknown => "unknown",
        };
        f.write_str(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiverKind {
    Unifying,
    Bolt,
    Lightspeed,
    Nano,
    Unknown,
}

impl fmt::Display for ReceiverKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Unifying => "unifying",
            Self::Bolt => "bolt",
            Self::Lightspeed => "lightspeed",
            Self::Nano => "nano",
            Self::Unknown => "unknown",
        };
        f.write_str(value)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Supported,
    Unsupported,
    #[default]
    Unknown,
}

impl CapabilityState {
    pub fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }
}

impl fmt::Display for CapabilityState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        };
        f.write_str(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub paired_device: Option<PairedDeviceInfo>,
    pub manufacturer: Option<String>,
    pub serial_number: Option<String>,
    pub bus: BusKind,
    pub bus_id: Option<u16>,
    pub vendor_id: u16,
    pub product_id: u16,
    pub release_number: Option<u16>,
    pub connection: ConnectionKind,
    pub receiver_kind: Option<ReceiverKind>,
    pub path: String,
    pub sysfs_path: String,
    pub physical_path: Option<String>,
    pub driver: Option<String>,
    pub interface_number: Option<i32>,
    pub usage_page: Option<u16>,
    pub usage: Option<u16>,
    pub access: DeviceAccess,
    pub battery: BatteryInfo,
    pub report_descriptor: ReportDescriptorInfo,
    pub capabilities: DeviceCapabilities,
}

impl DeviceInfo {
    pub fn product_key(&self) -> String {
        format!("{:04x}:{:04x}", self.vendor_id, self.product_id)
    }

    pub fn is_logitech(&self) -> bool {
        is_logitech_vendor(self.vendor_id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PairedDeviceInfo {
    pub slot: u8,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub wpid: Option<String>,
    pub protocol: Option<HidppProtocolVersion>,
    pub unit_id: Option<String>,
    pub model_id: Option<String>,
    pub feature_count: usize,
    pub features: Vec<HidppFeatureInfo>,
}

impl PairedDeviceInfo {
    pub fn display_name(&self) -> Option<&str> {
        self.name.as_deref().filter(|name| !name.is_empty())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HidppProtocolVersion {
    pub major: u8,
    pub minor: u8,
}

impl fmt::Display for HidppProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HidppFeatureInfo {
    pub index: u8,
    pub feature_id: u16,
    pub name: String,
    pub flags: u8,
    pub version: u8,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeviceAccess {
    pub sysfs_readable: bool,
    pub hidraw_readable: bool,
    pub hidraw_readwrite: bool,
    pub write_policy: WritePolicy,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WritePolicy {
    #[default]
    Disabled,
    ExplicitApplyOnly,
}

impl fmt::Display for WritePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("disabled"),
            Self::ExplicitApplyOnly => f.write_str("explicit apply only"),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct BatteryInfo {
    pub level_percent: Option<u8>,
    pub status: BatteryStatus,
    pub source: BatterySource,
    pub detail: Option<String>,
}

impl BatteryInfo {
    pub fn not_queried(reason: impl Into<String>) -> Self {
        Self {
            level_percent: None,
            status: BatteryStatus::Unknown,
            source: BatterySource::NotQueried,
            detail: Some(reason.into()),
        }
    }

    pub fn summary(&self) -> String {
        match self.level_percent {
            Some(level) => format!("{level}% {}", self.status),
            None => self
                .detail
                .clone()
                .unwrap_or_else(|| self.status.to_string()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatteryStatus {
    Charging,
    Discharging,
    Full,
    Low,
    Critical,
    Offline,
    #[default]
    Unknown,
}

impl fmt::Display for BatteryStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Charging => "charging",
            Self::Discharging => "discharging",
            Self::Full => "full",
            Self::Low => "low",
            Self::Critical => "critical",
            Self::Offline => "offline",
            Self::Unknown => "unknown",
        };
        f.write_str(value)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatterySource {
    Hidpp,
    #[default]
    NotQueried,
    Unavailable,
}

impl fmt::Display for BatterySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Hidpp => "hidpp",
            Self::NotQueried => "not queried",
            Self::Unavailable => "unavailable",
        };
        f.write_str(value)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct HidUsage {
    pub usage_page: u16,
    pub usage: u16,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReportDescriptorInfo {
    pub byte_len: usize,
    pub report_ids: Vec<u8>,
    pub has_input_reports: bool,
    pub has_output_reports: bool,
    pub has_feature_reports: bool,
    pub vendor_usage_pages: Vec<u16>,
    pub hidpp_usage: Option<HidUsage>,
}

impl ReportDescriptorInfo {
    pub fn is_hidpp_interface(&self) -> bool {
        self.hidpp_usage
            .as_ref()
            .is_some_and(|usage| is_hidpp_usage(Some(usage.usage_page), Some(usage.usage)))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeviceCapabilities {
    pub hidpp: CapabilityState,
    pub battery: CapabilityState,
    pub dpi: CapabilityState,
    pub button_mapping: CapabilityState,
    pub onboard_profiles: CapabilityState,
    pub wheel_mode: CapabilityState,
}

impl DeviceCapabilities {
    pub fn from_hidpp_detection(hidpp_detected: bool) -> Self {
        let hidpp = if hidpp_detected {
            CapabilityState::Supported
        } else {
            CapabilityState::Unknown
        };

        Self {
            hidpp,
            battery: CapabilityState::Unknown,
            dpi: CapabilityState::Unknown,
            button_mapping: CapabilityState::Unknown,
            onboard_profiles: CapabilityState::Unknown,
            wheel_mode: CapabilityState::Unknown,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub active_dpi: Option<u16>,
}

pub fn is_logitech_vendor(vendor_id: u16) -> bool {
    vendor_id == LOGITECH_VENDOR_ID
}

pub fn resolved_logitech_device_name(device: &DeviceInfo) -> Option<&str> {
    device
        .paired_device
        .as_ref()
        .and_then(resolved_logitech_paired_device_name)
        .or_else(|| known_logitech_product_name(device.vendor_id, device.product_id))
}

pub fn resolved_logitech_paired_device_name(paired: &PairedDeviceInfo) -> Option<&str> {
    paired
        .display_name()
        .or_else(|| known_logitech_wpid_name(paired.wpid.as_deref()))
        .or_else(|| known_logitech_model_name(paired.model_id.as_deref()))
}

pub fn known_logitech_wpid_name(wpid: Option<&str>) -> Option<&'static str> {
    known_logitech_model_code_name(wpid?)
}

pub fn known_logitech_model_name(model_id: Option<&str>) -> Option<&'static str> {
    known_logitech_model_code_name(model_id?)
}

fn known_logitech_model_code_name(value: &str) -> Option<&'static str> {
    let normalized = normalized_hidpp_model_code(value);
    if normalized.starts_with("B034") {
        Some("MX Master 3S")
    } else if normalized.starts_with("B035") {
        Some("MX Master 3S for Business")
    } else {
        None
    }
}

pub fn known_logitech_product_name(vendor_id: u16, product_id: u16) -> Option<&'static str> {
    if !is_logitech_vendor(vendor_id) {
        return None;
    }

    match product_id {
        0xb034 => Some("MX Master 3S"),
        0xb035 => Some("MX Master 3S for Business"),
        _ => None,
    }
}

pub fn stable_device_id(vendor_id: u16, product_id: u16, path: &str) -> String {
    format!(
        "{vendor_id:04x}:{product_id:04x}:{:016x}",
        fnv1a64(path.as_bytes())
    )
}

pub fn device_settings_id(device: &DeviceInfo) -> String {
    let unit_id = device
        .paired_device
        .as_ref()
        .and_then(|paired| paired.unit_id.as_deref())
        .map(normalized_hidpp_model_code)
        .filter(|unit_id| !unit_id.is_empty() && unit_id.bytes().any(|byte| byte != b'0'));

    match unit_id {
        Some(unit_id) => format!("{:04x}:unit:{unit_id}", device.vendor_id),
        None => device.id.clone(),
    }
}

pub fn infer_receiver_kind(product_id: u16, product_name: Option<&str>) -> Option<ReceiverKind> {
    match product_id {
        0xc52b | 0xc532 | 0xc534 => Some(ReceiverKind::Unifying),
        0xc548 => Some(ReceiverKind::Bolt),
        0xc539 | 0xc53a | 0xc547 => Some(ReceiverKind::Lightspeed),
        0xc517 | 0xc518 | 0xc51a | 0xc521 | 0xc531 => Some(ReceiverKind::Nano),
        _ => {
            let name = product_name.unwrap_or_default().to_ascii_lowercase();
            if name.contains("bolt") {
                Some(ReceiverKind::Bolt)
            } else if name.contains("unifying") {
                Some(ReceiverKind::Unifying)
            } else if name.contains("lightspeed") {
                Some(ReceiverKind::Lightspeed)
            } else if name.contains("receiver") {
                Some(ReceiverKind::Unknown)
            } else {
                None
            }
        }
    }
}

pub fn infer_connection(product_id: u16, product_name: Option<&str>, path: &str) -> ConnectionKind {
    match infer_receiver_kind(product_id, product_name) {
        Some(ReceiverKind::Unifying) => return ConnectionKind::Unifying,
        Some(ReceiverKind::Bolt) => return ConnectionKind::Bolt,
        Some(ReceiverKind::Lightspeed) => return ConnectionKind::Lightspeed,
        Some(ReceiverKind::Nano | ReceiverKind::Unknown) => return ConnectionKind::Usb,
        None => {}
    }

    let haystack = format!(
        "{} {}",
        product_name.unwrap_or_default().to_ascii_lowercase(),
        path.to_ascii_lowercase()
    );

    if haystack.contains("bluetooth") || haystack.contains("bluez") {
        ConnectionKind::Bluetooth
    } else if !path.is_empty() {
        ConnectionKind::Usb
    } else {
        ConnectionKind::Unknown
    }
}

pub fn bus_kind_from_linux_bus_id(bus_id: u16) -> BusKind {
    match bus_id {
        0x0003 => BusKind::Usb,
        0x0005 => BusKind::Bluetooth,
        0x0006 => BusKind::Virtual,
        0x0018 => BusKind::I2c,
        0x001c => BusKind::Spi,
        _ => BusKind::Unknown,
    }
}

pub fn is_hidpp_usage(usage_page: Option<u16>, usage: Option<u16>) -> bool {
    matches!(usage_page, Some(0xff00)) && matches!(usage, Some(0x0001 | 0x0002))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn normalized_hidpp_model_code(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .flat_map(char::to_uppercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_id_is_repeatable() {
        assert_eq!(
            stable_device_id(0x046d, 0xc548, "/dev/hidraw3"),
            stable_device_id(0x046d, 0xc548, "/dev/hidraw3")
        );
    }

    #[test]
    fn settings_id_prefers_normalized_paired_unit_id() {
        let mut device = test_device();
        device.paired_device = Some(PairedDeviceInfo {
            slot: 1,
            unit_id: Some("aa-bb-cc-dd".to_owned()),
            name: None,
            kind: None,
            wpid: None,
            protocol: None,
            model_id: None,
            feature_count: 0,
            features: Vec::new(),
        });

        assert_eq!(device_settings_id(&device), "046d:unit:AABBCCDD");
    }

    #[test]
    fn settings_id_falls_back_for_missing_or_zero_unit_id() {
        let mut device = test_device();
        assert_eq!(device_settings_id(&device), "receiver-endpoint");

        device.paired_device = Some(PairedDeviceInfo {
            slot: 1,
            unit_id: Some("00000000".to_owned()),
            name: None,
            kind: None,
            wpid: None,
            protocol: None,
            model_id: None,
            feature_count: 0,
            features: Vec::new(),
        });
        assert_eq!(device_settings_id(&device), "receiver-endpoint");
    }

    fn test_device() -> DeviceInfo {
        DeviceInfo {
            id: "receiver-endpoint".to_owned(),
            name: "Logi Bolt Receiver".to_owned(),
            paired_device: None,
            manufacturer: Some("Logitech".to_owned()),
            serial_number: None,
            bus: BusKind::Usb,
            bus_id: Some(0x0003),
            vendor_id: LOGITECH_VENDOR_ID,
            product_id: 0xc548,
            release_number: None,
            connection: ConnectionKind::Bolt,
            receiver_kind: Some(ReceiverKind::Bolt),
            path: "/dev/hidraw2".to_owned(),
            sysfs_path: "/sys/class/hidraw/hidraw2".to_owned(),
            physical_path: None,
            driver: None,
            interface_number: None,
            usage_page: None,
            usage: None,
            access: DeviceAccess::default(),
            battery: BatteryInfo::default(),
            report_descriptor: ReportDescriptorInfo::default(),
            capabilities: DeviceCapabilities::default(),
        }
    }

    #[test]
    fn receiver_kind_infers_common_receivers() {
        assert_eq!(
            infer_receiver_kind(0xc52b, None),
            Some(ReceiverKind::Unifying)
        );
        assert_eq!(infer_receiver_kind(0xc548, None), Some(ReceiverKind::Bolt));
        assert_eq!(
            infer_receiver_kind(0x0000, Some("USB Receiver LIGHTSPEED")),
            Some(ReceiverKind::Lightspeed)
        );
    }

    #[test]
    fn known_logitech_names_resolve_master3s_ids() {
        assert_eq!(known_logitech_wpid_name(Some("B034")), Some("MX Master 3S"));
        assert_eq!(
            known_logitech_model_name(Some("B03400000000")),
            Some("MX Master 3S")
        );
        assert_eq!(
            known_logitech_model_name(Some("b034-0000-0000")),
            Some("MX Master 3S")
        );
        assert_eq!(
            known_logitech_product_name(LOGITECH_VENDOR_ID, 0xb034),
            Some("MX Master 3S")
        );
        assert_eq!(
            known_logitech_product_name(LOGITECH_VENDOR_ID, 0xb035),
            Some("MX Master 3S for Business")
        );
    }

    #[test]
    fn known_logitech_names_do_not_map_bolt_receiver_to_mouse() {
        assert_eq!(
            known_logitech_product_name(LOGITECH_VENDOR_ID, 0xc548),
            None
        );
    }

    #[test]
    fn hidpp_usage_is_conservative() {
        assert!(is_hidpp_usage(Some(0xff00), Some(0x0001)));
        assert!(is_hidpp_usage(Some(0xff00), Some(0x0002)));
        assert!(!is_hidpp_usage(Some(0x0001), Some(0x0002)));
    }
}
