use std::collections::HashMap;

use dogi_core::{DogiError, Result};
use dogi_ui::{ApplicationLanguage, ApplicationUpdateNotification};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::Value;

const APPLICATION_ID: &str = "io.github.oksyd.dogi";
const APPLICATION_NAME: &str = "Dogi";
const NOTIFICATION_SERVICE: &str = "org.freedesktop.Notifications";
const NOTIFICATION_PATH: &str = "/org/freedesktop/Notifications";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BatteryNotificationLevel {
    Low,
    Critical,
}

pub(crate) fn show_update_ready(notification: ApplicationUpdateNotification) -> Result<()> {
    show(DesktopNotification {
        replaces_id: 0,
        title: notification.title,
        body: notification.body,
        urgency: NotificationUrgency::Normal,
    })
    .map(|_| ())
}

pub(crate) fn show_low_battery(
    language: ApplicationLanguage,
    device_name: &str,
    percent: u8,
    level: BatteryNotificationLevel,
    replaces_id: Option<u32>,
) -> Result<u32> {
    let critical = level == BatteryNotificationLevel::Critical;
    let (title, body) = low_battery_content(language, device_name, percent, level);

    show(DesktopNotification {
        replaces_id: replaces_id.unwrap_or_default(),
        title,
        body,
        urgency: if critical {
            NotificationUrgency::Critical
        } else {
            NotificationUrgency::Normal
        },
    })
}

pub(crate) fn show_fully_charged(
    language: ApplicationLanguage,
    device_name: &str,
    replaces_id: Option<u32>,
) -> Result<u32> {
    let (title, body) = fully_charged_content(language, device_name);
    show(DesktopNotification {
        replaces_id: replaces_id.unwrap_or_default(),
        title,
        body,
        urgency: NotificationUrgency::Normal,
    })
}

fn low_battery_content(
    language: ApplicationLanguage,
    device_name: &str,
    percent: u8,
    level: BatteryNotificationLevel,
) -> (String, String) {
    let critical = level == BatteryNotificationLevel::Critical;
    if language.locale().starts_with("zh") {
        if critical {
            (
                format!("{device_name} 电量严重不足"),
                format!("仅剩 {percent}%，请连接充电线。"),
            )
        } else {
            (
                format!("{device_name} 电量低"),
                format!("剩余 {percent}%，请及时充电。"),
            )
        }
    } else if critical {
        (
            format!("{device_name} battery is critically low"),
            format!("Only {percent}% remains. Connect a charging cable."),
        )
    } else {
        (
            format!("{device_name} battery is low"),
            format!("{percent}% remains. Charge the device soon."),
        )
    }
}

fn fully_charged_content(language: ApplicationLanguage, device_name: &str) -> (String, String) {
    if language.locale().starts_with("zh") {
        (
            format!("{device_name} 已充满"),
            "充电已完成，可以拔下充电线。".to_owned(),
        )
    } else {
        (
            format!("{device_name} is fully charged"),
            "Charging is complete. You can disconnect the cable.".to_owned(),
        )
    }
}

pub(crate) fn close(notification_id: u32) -> Result<()> {
    let connection = Connection::session().map_err(notification_error)?;
    let proxy = notification_proxy(&connection)?;
    proxy
        .call::<_, _, ()>("CloseNotification", &(notification_id,))
        .map_err(notification_error)
}

#[derive(Clone, Copy)]
enum NotificationUrgency {
    Normal = 1,
    Critical = 2,
}

struct DesktopNotification {
    replaces_id: u32,
    title: String,
    body: String,
    urgency: NotificationUrgency,
}

fn show(notification: DesktopNotification) -> Result<u32> {
    let connection = Connection::session().map_err(notification_error)?;
    let proxy = notification_proxy(&connection)?;
    let actions: Vec<&str> = Vec::new();
    let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
    hints.insert("desktop-entry", Value::new(APPLICATION_ID));
    hints.insert("urgency", Value::new(notification.urgency as u8));
    proxy
        .call(
            "Notify",
            &(
                APPLICATION_NAME,
                notification.replaces_id,
                APPLICATION_ID,
                notification.title,
                notification.body,
                actions,
                hints,
                -1_i32,
            ),
        )
        .map_err(notification_error)
}

fn notification_proxy(connection: &Connection) -> Result<Proxy<'_>> {
    Proxy::new(
        connection,
        NOTIFICATION_SERVICE,
        NOTIFICATION_PATH,
        NOTIFICATION_SERVICE,
    )
    .map_err(notification_error)
}

fn notification_error(error: impl std::fmt::Display) -> DogiError {
    DogiError::BackendUnavailable(format!("desktop notifications are unavailable: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_battery_copy_is_localized_and_device_specific() {
        assert_eq!(
            low_battery_content(
                ApplicationLanguage::SimplifiedChinese,
                "MX Master 3S",
                18,
                BatteryNotificationLevel::Low,
            ),
            (
                "MX Master 3S 电量低".to_owned(),
                "剩余 18%，请及时充电。".to_owned(),
            )
        );
        assert_eq!(
            low_battery_content(
                ApplicationLanguage::English,
                "MX Master 3S",
                8,
                BatteryNotificationLevel::Critical,
            ),
            (
                "MX Master 3S battery is critically low".to_owned(),
                "Only 8% remains. Connect a charging cable.".to_owned(),
            )
        );
    }

    #[test]
    fn fully_charged_copy_is_localized_and_device_specific() {
        assert_eq!(
            fully_charged_content(ApplicationLanguage::SimplifiedChinese, "MX Master 3S"),
            (
                "MX Master 3S 已充满".to_owned(),
                "充电已完成，可以拔下充电线。".to_owned(),
            )
        );
        assert_eq!(
            fully_charged_content(ApplicationLanguage::English, "MX Master 3S"),
            (
                "MX Master 3S is fully charged".to_owned(),
                "Charging is complete. You can disconnect the cable.".to_owned(),
            )
        );
    }
}
