use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dogi_core::{BatteryInfo, BatterySource, BatteryStatus, DogiError, Result};
use dogi_hid::Master3sRuntimeEventListener;
use dogi_ui::ApplicationLanguage;
use serde::{Deserialize, Serialize};

use crate::desktop::notifications::{self, BatteryNotificationLevel};

pub(crate) const BATTERY_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(15 * 60);
pub(crate) const BATTERY_CONFIRMATION_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(30);
pub(crate) const BATTERY_CHARGING_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(2 * 60);
pub(crate) const BATTERY_RETRY_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(5 * 60);
const PREFERENCE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

const STATE_VERSION: u8 = 2;
const LOW_THRESHOLD_PERCENT: u8 = 20;
const CRITICAL_THRESHOLD_PERCENT: u8 = 10;
const RECOVERY_THRESHOLD_PERCENT: u8 = 25;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BatteryAlertLevel {
    Low,
    Critical,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BatteryAlertDecision {
    None,
    ClearLowBattery,
    NotifyLowBattery {
        level: BatteryAlertLevel,
        percent: u8,
    },
    NotifyFullyCharged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingObservationKind {
    LowBattery(BatteryAlertLevel),
    FullyCharged,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ChargeCycleState {
    #[default]
    Idle,
    Charging,
    FullNotified,
}

#[derive(Debug)]
pub(crate) struct BatteryNotificationMonitor {
    path: PathBuf,
    state: StoredBatteryNotificationState,
    pending: HashMap<String, PendingObservation>,
    notification_ids: HashMap<String, u32>,
    active_device: Option<String>,
    low_battery_notifications_enabled: bool,
    full_battery_notifications_enabled: bool,
    next_check: Instant,
    next_preference_check: Instant,
}

impl BatteryNotificationMonitor {
    pub(crate) fn load(path: PathBuf) -> Result<Self> {
        let state = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<StoredBatteryNotificationState>(&bytes).map_err(
                |error| {
                    DogiError::Config(format!(
                        "failed to decode battery notification state {}: {error}",
                        path.display()
                    ))
                },
            )?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                StoredBatteryNotificationState::default()
            }
            Err(error) => {
                return Err(DogiError::Config(format!(
                    "failed to read battery notification state {}: {error}",
                    path.display()
                )));
            }
        };
        if state.version != STATE_VERSION {
            return Err(DogiError::Config(format!(
                "unsupported battery notification state version {}; expected {STATE_VERSION}",
                state.version
            )));
        }
        Ok(Self {
            path,
            state,
            pending: HashMap::new(),
            notification_ids: HashMap::new(),
            active_device: None,
            low_battery_notifications_enabled: true,
            full_battery_notifications_enabled: true,
            next_check: Instant::now(),
            next_preference_check: Instant::now(),
        })
    }

    pub(crate) fn empty(path: PathBuf) -> Self {
        Self {
            path,
            state: StoredBatteryNotificationState::default(),
            pending: HashMap::new(),
            notification_ids: HashMap::new(),
            active_device: None,
            low_battery_notifications_enabled: true,
            full_battery_notifications_enabled: true,
            next_check: Instant::now(),
            next_preference_check: Instant::now(),
        }
    }

    pub(crate) fn activate(&mut self, device_id: &str) {
        if self.active_device.as_deref() != Some(device_id) {
            self.active_device = Some(device_id.to_owned());
            self.next_check = Instant::now();
            self.next_preference_check = Instant::now();
        }
    }

    pub(crate) fn is_due(&self) -> bool {
        Instant::now() >= self.next_check
    }

    pub(crate) fn preferences_due(&self) -> bool {
        Instant::now() >= self.next_preference_check
    }

    pub(crate) fn update_preferences(
        &mut self,
        device_id: &str,
        low_battery_enabled: bool,
        full_battery_enabled: bool,
    ) -> Result<()> {
        self.next_preference_check = Instant::now() + PREFERENCE_POLL_INTERVAL;
        let low_battery_changed = self.low_battery_notifications_enabled != low_battery_enabled;
        let full_battery_changed = self.full_battery_notifications_enabled != full_battery_enabled;
        if !low_battery_changed && !full_battery_changed {
            return Ok(());
        }

        self.low_battery_notifications_enabled = low_battery_enabled;
        self.full_battery_notifications_enabled = full_battery_enabled;
        if low_battery_changed && !low_battery_enabled {
            self.clear_low_battery(device_id)?;
            self.close_active_notification(device_id);
        }
        if full_battery_changed && !full_battery_enabled {
            self.remove_pending(device_id, PendingObservationKind::FullyCharged);
        }
        if low_battery_enabled || full_battery_enabled {
            self.next_check = Instant::now();
        }
        Ok(())
    }

    pub(crate) fn read_timeout(&self, maximum: Duration) -> Duration {
        self.next_check
            .checked_duration_since(Instant::now())
            .unwrap_or_default()
            .min(
                self.next_preference_check
                    .checked_duration_since(Instant::now())
                    .unwrap_or_default(),
            )
            .min(maximum)
    }

    pub(crate) fn check_if_due(
        &mut self,
        listener: &mut Master3sRuntimeEventListener,
        device_id: &str,
        device_name: &str,
        language: ApplicationLanguage,
    ) -> Result<()> {
        if Instant::now() < self.next_check {
            return Ok(());
        }

        if !self.low_battery_notifications_enabled && !self.full_battery_notifications_enabled {
            self.close_active_notification(device_id);
            self.next_check = Instant::now() + BATTERY_POLL_INTERVAL;
            return Ok(());
        }

        let battery = match listener.read_battery() {
            Ok(Some(battery)) => battery,
            Ok(None) => {
                self.next_check = Instant::now() + BATTERY_RETRY_INTERVAL;
                return Ok(());
            }
            Err(error) => {
                self.next_check = Instant::now() + BATTERY_RETRY_INTERVAL;
                return Err(error);
            }
        };
        let decision = match self.observe(device_id, &battery) {
            Ok(decision) => decision,
            Err(error) => {
                self.next_check = Instant::now() + BATTERY_RETRY_INTERVAL;
                return Err(error);
            }
        };

        let outcome = match decision {
            BatteryAlertDecision::None => Ok(()),
            BatteryAlertDecision::ClearLowBattery => {
                self.close_active_notification(device_id);
                Ok(())
            }
            BatteryAlertDecision::NotifyLowBattery { level, percent } => {
                let notification_level = match level {
                    BatteryAlertLevel::Low => BatteryNotificationLevel::Low,
                    BatteryAlertLevel::Critical => BatteryNotificationLevel::Critical,
                };
                notifications::show_low_battery(
                    language,
                    device_name,
                    percent,
                    notification_level,
                    self.notification_ids.get(device_id).copied(),
                )
                .and_then(|notification_id| {
                    self.notification_ids
                        .insert(device_id.to_owned(), notification_id);
                    self.mark_low_battery_notified(device_id, level)
                })
            }
            BatteryAlertDecision::NotifyFullyCharged => {
                let replaces_id = self.notification_ids.get(device_id).copied();
                notifications::show_fully_charged(language, device_name, replaces_id).and_then(
                    |_| {
                        self.notification_ids.remove(device_id);
                        self.mark_fully_charged_notified(device_id)
                    },
                )
            }
        };
        self.next_check = Instant::now()
            + if outcome.is_err() {
                BATTERY_RETRY_INTERVAL
            } else if self.awaiting_confirmation(device_id) {
                BATTERY_CONFIRMATION_INTERVAL
            } else if self.charging(device_id) {
                BATTERY_CHARGING_POLL_INTERVAL
            } else {
                BATTERY_POLL_INTERVAL
            };
        outcome
    }

    pub(crate) fn observe(
        &mut self,
        device_id: &str,
        battery: &BatteryInfo,
    ) -> Result<BatteryAlertDecision> {
        let Some(sample) = BatterySample::from_info(battery) else {
            self.pending.remove(device_id);
            return Ok(BatteryAlertDecision::None);
        };

        let mut device_state = self.device_state(device_id);
        let mut decision = BatteryAlertDecision::None;

        match sample.status {
            BatteryStatus::Charging => {
                self.remove_pending(device_id, PendingObservationKind::FullyCharged);
                if device_state.charge_cycle == ChargeCycleState::Idle {
                    device_state.charge_cycle = ChargeCycleState::Charging;
                }
            }
            BatteryStatus::Full => {
                if device_state.charge_cycle == ChargeCycleState::Charging {
                    if self.full_battery_notifications_enabled {
                        if self.confirm(device_id, PendingObservationKind::FullyCharged) {
                            decision = BatteryAlertDecision::NotifyFullyCharged;
                        }
                    } else {
                        self.remove_pending(device_id, PendingObservationKind::FullyCharged);
                        device_state.charge_cycle = ChargeCycleState::FullNotified;
                    }
                } else {
                    self.remove_pending(device_id, PendingObservationKind::FullyCharged);
                }
            }
            BatteryStatus::Discharging | BatteryStatus::Low | BatteryStatus::Critical => {
                self.remove_pending(device_id, PendingObservationKind::FullyCharged);
                device_state.charge_cycle = ChargeCycleState::Idle;
            }
            BatteryStatus::Unknown => {
                self.remove_pending(device_id, PendingObservationKind::FullyCharged);
            }
            BatteryStatus::Offline => unreachable!("offline samples are filtered"),
        }

        let low_battery_recovered = sample.recovered();
        if !self.low_battery_notifications_enabled || low_battery_recovered {
            self.remove_pending_low_battery(device_id);
            if device_state.low_battery_alert.take().is_some()
                && decision == BatteryAlertDecision::None
            {
                decision = BatteryAlertDecision::ClearLowBattery;
            }
        } else if let Some((level, percent)) = sample.alert() {
            let should_notify = match (device_state.low_battery_alert, level) {
                (None, _) | (Some(BatteryAlertLevel::Low), BatteryAlertLevel::Critical) => true,
                (Some(BatteryAlertLevel::Low), BatteryAlertLevel::Low)
                | (Some(BatteryAlertLevel::Critical), _) => false,
            };
            if should_notify {
                if self.confirm(device_id, PendingObservationKind::LowBattery(level)) {
                    decision = BatteryAlertDecision::NotifyLowBattery { level, percent };
                }
            } else {
                self.remove_pending_low_battery(device_id);
            }
        } else {
            self.remove_pending_low_battery(device_id);
        }

        self.set_device_state(device_id, device_state)?;
        Ok(decision)
    }

    pub(crate) fn mark_low_battery_notified(
        &mut self,
        device_id: &str,
        level: BatteryAlertLevel,
    ) -> Result<()> {
        self.pending.remove(device_id);
        let mut state = self.device_state(device_id);
        state.low_battery_alert = Some(level);
        self.set_device_state(device_id, state).map(|_| ())
    }

    pub(crate) fn mark_fully_charged_notified(&mut self, device_id: &str) -> Result<()> {
        self.remove_pending(device_id, PendingObservationKind::FullyCharged);
        let mut state = self.device_state(device_id);
        state.charge_cycle = ChargeCycleState::FullNotified;
        self.set_device_state(device_id, state).map(|_| ())
    }

    pub(crate) fn awaiting_confirmation(&self, device_id: &str) -> bool {
        self.pending.contains_key(device_id)
    }

    fn charging(&self, device_id: &str) -> bool {
        self.device_state(device_id).charge_cycle == ChargeCycleState::Charging
    }

    fn confirm(&mut self, device_id: &str, kind: PendingObservationKind) -> bool {
        let observation = self
            .pending
            .entry(device_id.to_owned())
            .or_insert(PendingObservation { kind, count: 0 });
        if observation.kind != kind {
            *observation = PendingObservation { kind, count: 0 };
        }
        observation.count = observation.count.saturating_add(1);
        observation.count >= 2
    }

    fn remove_pending(&mut self, device_id: &str, kind: PendingObservationKind) {
        if self.pending.get(device_id).map(|pending| pending.kind) == Some(kind) {
            self.pending.remove(device_id);
        }
    }

    fn remove_pending_low_battery(&mut self, device_id: &str) {
        if matches!(
            self.pending.get(device_id),
            Some(PendingObservation {
                kind: PendingObservationKind::LowBattery(_),
                ..
            })
        ) {
            self.pending.remove(device_id);
        }
    }

    fn clear_low_battery(&mut self, device_id: &str) -> Result<bool> {
        self.remove_pending_low_battery(device_id);
        let mut state = self.device_state(device_id);
        let changed = state.low_battery_alert.take().is_some();
        self.set_device_state(device_id, state)?;
        Ok(changed)
    }

    fn device_state(&self, device_id: &str) -> StoredDeviceNotificationState {
        self.state
            .devices
            .get(device_id)
            .copied()
            .unwrap_or_default()
    }

    fn set_device_state(
        &mut self,
        device_id: &str,
        state: StoredDeviceNotificationState,
    ) -> Result<bool> {
        let previous = self.device_state(device_id);
        if previous == state {
            return Ok(false);
        }
        if state == StoredDeviceNotificationState::default() {
            self.state.devices.remove(device_id);
        } else {
            self.state.devices.insert(device_id.to_owned(), state);
        }
        self.save()?;
        Ok(true)
    }

    fn close_active_notification(&mut self, device_id: &str) {
        if let Some(notification_id) = self.notification_ids.remove(device_id) {
            let _ = notifications::close(notification_id);
        }
    }

    fn save(&self) -> Result<()> {
        let parent = self.path.parent().ok_or_else(|| {
            DogiError::Config(format!(
                "battery notification state path has no parent: {}",
                self.path.display()
            ))
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            DogiError::Config(format!(
                "failed to create battery notification cache {}: {error}",
                parent.display()
            ))
        })?;
        let temporary_path = temporary_path(&self.path);
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary_path)
            .map_err(|error| state_write_error(&temporary_path, error))?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, &self.state).map_err(|error| {
            DogiError::Config(format!(
                "failed to encode battery notification state {}: {error}",
                temporary_path.display()
            ))
        })?;
        writer
            .write_all(b"\n")
            .and_then(|()| writer.flush())
            .map_err(|error| state_write_error(&temporary_path, error))?;
        writer
            .into_inner()
            .map_err(|error| state_write_error(&temporary_path, error.into_error()))?
            .sync_all()
            .map_err(|error| state_write_error(&temporary_path, error))?;
        fs::rename(&temporary_path, &self.path)
            .map_err(|error| state_write_error(&self.path, error))?;
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingObservation {
    kind: PendingObservationKind,
    count: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BatterySample {
    percent: Option<u8>,
    status: BatteryStatus,
}

impl BatterySample {
    fn from_info(battery: &BatteryInfo) -> Option<Self> {
        (battery.source == BatterySource::Hidpp)
            .then_some(Self {
                percent: battery.level_percent,
                status: battery.status,
            })
            .filter(|sample| sample.status != BatteryStatus::Offline)
    }

    fn recovered(self) -> bool {
        matches!(self.status, BatteryStatus::Charging | BatteryStatus::Full)
            || self
                .percent
                .is_some_and(|percent| percent >= RECOVERY_THRESHOLD_PERCENT)
    }

    fn alert(self) -> Option<(BatteryAlertLevel, u8)> {
        let percent = self.percent?;
        if percent <= CRITICAL_THRESHOLD_PERCENT {
            Some((BatteryAlertLevel::Critical, percent))
        } else if percent <= LOW_THRESHOLD_PERCENT {
            Some((BatteryAlertLevel::Low, percent))
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDeviceNotificationState {
    low_battery_alert: Option<BatteryAlertLevel>,
    charge_cycle: ChargeCycleState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredBatteryNotificationState {
    version: u8,
    devices: BTreeMap<String, StoredDeviceNotificationState>,
}

impl Default for StoredBatteryNotificationState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            devices: BTreeMap::new(),
        }
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()))
}

fn state_write_error(path: &Path, error: io::Error) -> DogiError {
    DogiError::Config(format!(
        "failed to write battery notification state {}: {error}",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_and_critical_alerts_require_confirmation_and_only_escalate() {
        let path = unique_path("tiers");
        let mut monitor = BatteryNotificationMonitor::empty(path.clone());
        let low = battery(20, BatteryStatus::Low);
        let critical = battery(10, BatteryStatus::Critical);

        assert_eq!(
            monitor.observe("mouse", &low).unwrap(),
            BatteryAlertDecision::None
        );
        assert_eq!(
            monitor.observe("mouse", &low).unwrap(),
            BatteryAlertDecision::NotifyLowBattery {
                level: BatteryAlertLevel::Low,
                percent: 20,
            }
        );
        monitor
            .mark_low_battery_notified("mouse", BatteryAlertLevel::Low)
            .unwrap();
        assert_eq!(
            monitor.observe("mouse", &low).unwrap(),
            BatteryAlertDecision::None
        );
        assert_eq!(
            monitor.observe("mouse", &critical).unwrap(),
            BatteryAlertDecision::None
        );
        assert_eq!(
            monitor.observe("mouse", &critical).unwrap(),
            BatteryAlertDecision::NotifyLowBattery {
                level: BatteryAlertLevel::Critical,
                percent: 10,
            }
        );
        monitor
            .mark_low_battery_notified("mouse", BatteryAlertLevel::Critical)
            .unwrap();
        assert_eq!(
            monitor.observe("mouse", &critical).unwrap(),
            BatteryAlertDecision::None
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn charging_and_recovery_rearm_the_device() {
        let path = unique_path("recovery");
        let mut monitor = BatteryNotificationMonitor::empty(path.clone());
        monitor
            .observe("mouse", &battery(8, BatteryStatus::Critical))
            .unwrap();
        monitor
            .observe("mouse", &battery(8, BatteryStatus::Critical))
            .unwrap();
        monitor
            .mark_low_battery_notified("mouse", BatteryAlertLevel::Critical)
            .unwrap();

        assert_eq!(
            monitor
                .observe("mouse", &battery(9, BatteryStatus::Charging))
                .unwrap(),
            BatteryAlertDecision::ClearLowBattery
        );
        assert_eq!(
            monitor
                .observe("mouse", &battery(20, BatteryStatus::Low))
                .unwrap(),
            BatteryAlertDecision::None
        );
        assert!(monitor.awaiting_confirmation("mouse"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn unavailable_samples_never_change_the_alert_state() {
        let path = unique_path("unavailable");
        let mut monitor = BatteryNotificationMonitor::empty(path);
        let unavailable = BatteryInfo::not_queried("mouse is asleep");

        assert_eq!(
            monitor.observe("mouse", &unavailable).unwrap(),
            BatteryAlertDecision::None
        );
        assert!(!monitor.awaiting_confirmation("mouse"));
    }

    #[test]
    fn notified_state_survives_a_runtime_restart() {
        let path = unique_path("persistence");
        let mut monitor = BatteryNotificationMonitor::empty(path.clone());
        monitor
            .mark_low_battery_notified("mouse", BatteryAlertLevel::Low)
            .unwrap();

        let mut reloaded = BatteryNotificationMonitor::load(path.clone()).unwrap();
        assert_eq!(
            reloaded
                .observe("mouse", &battery(18, BatteryStatus::Low))
                .unwrap(),
            BatteryAlertDecision::None
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn disabling_notifications_clears_persisted_suppression() {
        let path = unique_path("disabled");
        let mut monitor = BatteryNotificationMonitor::empty(path.clone());
        monitor
            .mark_low_battery_notified("mouse", BatteryAlertLevel::Low)
            .unwrap();

        monitor.update_preferences("mouse", false, true).unwrap();
        let mut reloaded = BatteryNotificationMonitor::load(path.clone()).unwrap();
        assert_eq!(
            reloaded
                .observe("mouse", &battery(18, BatteryStatus::Low))
                .unwrap(),
            BatteryAlertDecision::None
        );
        assert!(reloaded.awaiting_confirmation("mouse"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn full_charge_requires_a_confirmed_transition_from_charging() {
        let path = unique_path("full-transition");
        let mut monitor = BatteryNotificationMonitor::empty(path.clone());
        let full = battery(100, BatteryStatus::Full);

        assert_eq!(
            monitor.observe("mouse", &full).unwrap(),
            BatteryAlertDecision::None
        );
        assert_eq!(
            monitor.observe("mouse", &full).unwrap(),
            BatteryAlertDecision::None
        );
        assert_eq!(
            monitor
                .observe("mouse", &battery(99, BatteryStatus::Charging))
                .unwrap(),
            BatteryAlertDecision::None
        );
        assert_eq!(
            monitor.observe("mouse", &full).unwrap(),
            BatteryAlertDecision::None
        );
        assert!(monitor.awaiting_confirmation("mouse"));
        assert_eq!(
            monitor.observe("mouse", &full).unwrap(),
            BatteryAlertDecision::NotifyFullyCharged
        );

        monitor.mark_fully_charged_notified("mouse").unwrap();
        assert_eq!(
            monitor.observe("mouse", &full).unwrap(),
            BatteryAlertDecision::None
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn full_charge_is_not_rearmed_until_the_device_discharges() {
        let path = unique_path("full-rearm");
        let mut monitor = BatteryNotificationMonitor::empty(path.clone());
        monitor
            .observe("mouse", &battery(95, BatteryStatus::Charging))
            .unwrap();
        monitor
            .observe("mouse", &battery(100, BatteryStatus::Full))
            .unwrap();
        monitor
            .observe("mouse", &battery(100, BatteryStatus::Full))
            .unwrap();
        monitor.mark_fully_charged_notified("mouse").unwrap();

        assert_eq!(
            monitor
                .observe("mouse", &battery(100, BatteryStatus::Charging))
                .unwrap(),
            BatteryAlertDecision::None
        );
        assert_eq!(
            monitor
                .observe("mouse", &battery(100, BatteryStatus::Full))
                .unwrap(),
            BatteryAlertDecision::None
        );

        monitor
            .observe("mouse", &battery(99, BatteryStatus::Discharging))
            .unwrap();
        monitor
            .observe("mouse", &battery(99, BatteryStatus::Charging))
            .unwrap();
        monitor
            .observe("mouse", &battery(100, BatteryStatus::Full))
            .unwrap();
        assert_eq!(
            monitor
                .observe("mouse", &battery(100, BatteryStatus::Full))
                .unwrap(),
            BatteryAlertDecision::NotifyFullyCharged
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn charging_state_survives_a_runtime_restart() {
        let path = unique_path("charging-persistence");
        let mut monitor = BatteryNotificationMonitor::empty(path.clone());
        monitor
            .observe("mouse", &battery(90, BatteryStatus::Charging))
            .unwrap();

        let mut reloaded = BatteryNotificationMonitor::load(path.clone()).unwrap();
        assert_eq!(
            reloaded
                .observe("mouse", &battery_status_only(BatteryStatus::Full))
                .unwrap(),
            BatteryAlertDecision::None
        );
        assert_eq!(
            reloaded
                .observe("mouse", &battery_status_only(BatteryStatus::Full))
                .unwrap(),
            BatteryAlertDecision::NotifyFullyCharged
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn disabling_full_notifications_consumes_the_current_charge_cycle() {
        let path = unique_path("full-disabled");
        let mut monitor = BatteryNotificationMonitor::empty(path.clone());
        monitor
            .observe("mouse", &battery(90, BatteryStatus::Charging))
            .unwrap();
        monitor.update_preferences("mouse", true, false).unwrap();
        assert_eq!(
            monitor
                .observe("mouse", &battery(100, BatteryStatus::Full))
                .unwrap(),
            BatteryAlertDecision::None
        );

        monitor.update_preferences("mouse", true, true).unwrap();
        assert_eq!(
            monitor
                .observe("mouse", &battery(100, BatteryStatus::Full))
                .unwrap(),
            BatteryAlertDecision::None
        );

        let _ = fs::remove_file(path);
    }

    fn battery(percent: u8, status: BatteryStatus) -> BatteryInfo {
        BatteryInfo {
            level_percent: Some(percent),
            status,
            source: BatterySource::Hidpp,
            detail: None,
        }
    }

    fn battery_status_only(status: BatteryStatus) -> BatteryInfo {
        BatteryInfo {
            level_percent: None,
            status,
            source: BatterySource::Hidpp,
            detail: None,
        }
    }

    fn unique_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "dogi-battery-notification-{label}-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
