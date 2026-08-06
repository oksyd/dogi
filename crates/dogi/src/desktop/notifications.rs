use std::collections::HashMap;

use dogi_core::{DogiError, Result};
use dogi_ui::ApplicationUpdateNotification;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::Value;

const APPLICATION_ID: &str = "io.github.oksyd.dogi";

pub(crate) fn show_update_ready(notification: ApplicationUpdateNotification) -> Result<()> {
    let connection = Connection::session().map_err(notification_error)?;
    let proxy = Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .map_err(notification_error)?;
    let actions: Vec<&str> = Vec::new();
    let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
    hints.insert("desktop-entry", Value::new(APPLICATION_ID));
    hints.insert("urgency", Value::new(1_u8));
    let _: u32 = proxy
        .call(
            "Notify",
            &(
                "dogi",
                0_u32,
                APPLICATION_ID,
                notification.title,
                notification.body,
                actions,
                hints,
                10_000_i32,
            ),
        )
        .map_err(notification_error)?;
    Ok(())
}

fn notification_error(error: impl std::fmt::Display) -> DogiError {
    DogiError::BackendUnavailable(format!("desktop notifications are unavailable: {error}"))
}
