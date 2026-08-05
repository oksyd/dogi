use std::rc::Rc;
use std::sync::Arc;

use dogi_core::{DogiError, Result};

use crate::config::application::ApplicationConfigStore;
use crate::desktop;
use crate::device::{ConfigFileOwner, DeviceService};
use crate::runtime::{control::RuntimeControlClient, service};

pub(crate) fn launch_gui() -> Result<()> {
    let devices = device_service_for_desktop_user();
    let preferences = application_preferences();
    let settings = devices.load_master3s_settings()?;
    let inventory_devices = devices.clone();
    let scan_devices = devices.clone();
    let load_settings = devices.clone();
    let save_settings = devices.clone();
    let apply_settings = devices;
    let preview_client =
        RuntimeControlClient::for_desktop_user().map_err(|error| error.to_string());

    dogi_ui::launch_with_integrations(
        dogi_ui::UiState::with_settings(Vec::new(), settings),
        dogi_ui::UiIntegrations {
            discovery: dogi_ui::DeviceDiscovery::new(
                Arc::new(move || inventory_devices.scan_device_inventory()),
                Arc::new(move || scan_devices.scan_devices_for_ui()),
            ),
            settings: dogi_ui::DeviceSettingsIntegration {
                load: Rc::new(move |settings_id| {
                    load_settings.load_master3s_settings_for_device(settings_id)
                }),
                save: Rc::new(move |device_id, settings| {
                    let path = match device_id {
                        Some(device_id) => {
                            save_settings.save_master3s_settings_for_device(device_id, settings)
                        }
                        None => save_settings.save_master3s_settings(settings),
                    }?;
                    Ok(path.display().to_string())
                }),
                apply: Rc::new(move |device_id, settings, plan| {
                    apply_settings.apply_master3s_settings_plan(device_id, settings, plan)
                }),
            },
            runtime: dogi_ui::DesktopRuntimeManager {
                manage: Arc::new(service::manage),
                horizontal_scroll_preview: Arc::new(move |command| {
                    let client = preview_client
                        .as_ref()
                        .map_err(|detail| DogiError::BackendUnavailable(detail.clone()))?;
                    match command {
                        dogi_ui::HorizontalScrollPreviewCommand::Set {
                            device_id,
                            speed_percent,
                        } => client.set_horizontal_scroll_preview(&device_id, speed_percent),
                        dogi_ui::HorizontalScrollPreviewCommand::Clear => {
                            client.clear_horizontal_scroll_preview()
                        }
                    }
                }),
            },
            preferences,
        },
    )
}

fn application_preferences() -> dogi_ui::ApplicationPreferencesIntegration {
    let store = match ApplicationConfigStore::from_environment() {
        Ok(store) => store,
        Err(error) => {
            let detail = error.to_string();
            let save_error = detail.clone();
            return dogi_ui::ApplicationPreferencesIntegration::new(
                dogi_ui::ApplicationPreferences::default(),
                move |_| Err(DogiError::Config(save_error.clone())),
            )
            .with_load_error(detail);
        }
    };

    let (initial, load_error) = match store.load_preferences() {
        Ok(preferences) => (preferences, None),
        Err(error) => (
            dogi_ui::ApplicationPreferences::default(),
            Some(error.to_string()),
        ),
    };
    let integration = dogi_ui::ApplicationPreferencesIntegration::new(initial, move |change| {
        store
            .save_preference(change)
            .map_err(|error| DogiError::Config(error.to_string()))
    });
    match load_error {
        Some(error) => integration.with_load_error(error),
        None => integration,
    }
}

fn device_service_for_desktop_user() -> DeviceService {
    let Some(context) = desktop::elevated_user() else {
        return DeviceService::new();
    };
    let (Some(uid), Some(gid)) = (context.uid, context.gid) else {
        return DeviceService::new();
    };
    DeviceService::with_owned_config_path(
        context.home.join(".config/dogi/master3s.json"),
        ConfigFileOwner::new(uid, gid),
    )
}
