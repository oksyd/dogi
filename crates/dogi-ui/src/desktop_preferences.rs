use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedValue;

use crate::MainWindow;

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.portal.Settings";
const APPEARANCE: &str = "org.freedesktop.appearance";
const CONTRAST: &str = "contrast";

pub(crate) fn watch_high_contrast(window: slint::Weak<MainWindow>) {
    let _ = std::thread::Builder::new()
        .name("dogi-desktop-preferences".to_owned())
        .spawn(move || watch_high_contrast_inner(window));
}

fn watch_high_contrast_inner(window: slint::Weak<MainWindow>) {
    let Ok(connection) = Connection::session() else {
        return;
    };
    let Ok(proxy) = Proxy::new(&connection, DESTINATION, PATH, INTERFACE) else {
        return;
    };

    let Ok(signals) =
        proxy.receive_signal_with_args("SettingChanged", &[(0, APPEARANCE), (1, CONTRAST)])
    else {
        return;
    };

    if let Ok(value) = proxy.call::<_, _, OwnedValue>("ReadOne", &(APPEARANCE, CONTRAST)) {
        update_window(&window, high_contrast_from_value(&value));
    }

    for message in signals {
        let Ok((namespace, key, value)) =
            message.body().deserialize::<(String, String, OwnedValue)>()
        else {
            continue;
        };
        if namespace == APPEARANCE && key == CONTRAST {
            update_window(&window, high_contrast_from_value(&value));
        }
    }
}

fn update_window(window: &slint::Weak<MainWindow>, high_contrast: bool) {
    let _ = window.upgrade_in_event_loop(move |window| {
        window.set_high_contrast(high_contrast);
    });
}

fn high_contrast_from_value(value: &OwnedValue) -> bool {
    value.downcast_ref::<u32>().is_ok_and(|value| value == 1)
}

#[cfg(test)]
mod tests {
    use super::high_contrast_from_value;
    use zbus::zvariant::OwnedValue;

    #[test]
    fn contrast_portal_values_are_conservative() {
        assert!(!high_contrast_from_value(&OwnedValue::from(0_u32)));
        assert!(high_contrast_from_value(&OwnedValue::from(1_u32)));
        assert!(!high_contrast_from_value(&OwnedValue::from(2_u32)));
    }
}
