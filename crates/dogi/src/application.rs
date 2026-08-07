use std::rc::Rc;
use std::sync::Arc;

use dogi_core::{DogiError, Result};

use crate::config::application::ApplicationConfigStore;
use crate::device::DeviceService;
use crate::environment::AppEnvironment;
use crate::runtime::{control::RuntimeControlClient, lock::ProcessLock, service};

pub(crate) fn launch_gui(environment: &AppEnvironment) -> Result<()> {
    if environment.user.uid.is_some() {
        return Err(DogiError::InvalidArgument(
            "run the Dogi GUI as the desktop user, without sudo".to_owned(),
        ));
    }
    let _instance_lock = ProcessLock::acquire(&environment.paths.gui_instance_lock(), "window")?;
    let devices = DeviceService::for_environment(environment);
    let settings_recovery_error = devices
        .recover_interrupted_settings_transaction()
        .err()
        .map(|error| error.to_string());
    let application_store = ApplicationConfigStore::for_environment(environment);
    let update_store = application_store.clone();
    let network_service = crate::network::NetworkService::new(application_store.clone());
    let network = network_preferences(network_service.clone());
    let preferences = application_preferences(application_store);
    let settings = devices.load_master3s_settings()?;
    let inventory_devices = devices.clone();
    let scan_devices = devices.clone();
    let load_settings = devices.clone();
    let save_settings = devices.clone();
    let prepare_settings = devices.clone();
    let commit_settings = devices;
    let preview_client =
        RuntimeControlClient::for_environment(environment).map_err(|error| error.to_string());
    let runtime_environment = environment.clone();
    let runtime_supported = environment.runtime.persistent_management_supported();
    let runtime_detail = environment.runtime.management_detail.clone();

    dogi_ui::launch_with_integrations(
        dogi_ui::UiState::with_settings(Vec::new(), settings),
        dogi_ui::UiIntegrations {
            identity: if environment.is_development() {
                dogi_ui::ApplicationIdentity::Development
            } else {
                dogi_ui::ApplicationIdentity::Stable
            },
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
                prepare: Arc::new(move |device_id, settings, plan| {
                    prepare_settings
                        .prepare_master3s_settings_transaction(device_id, settings, plan)
                }),
                commit: Arc::new(move |device_id, settings_id, settings, plan| {
                    let (report, path) = commit_settings.commit_prepared_master3s_settings(
                        device_id,
                        settings_id,
                        settings,
                        plan,
                    )?;
                    Ok(dogi_ui::SettingsCommitResult {
                        report,
                        saved_path: path.display().to_string(),
                    })
                }),
                recovery_error: settings_recovery_error,
            },
            runtime: dogi_ui::DesktopRuntimeManager {
                supported: runtime_supported,
                availability: if runtime_supported {
                    dogi_ui::DesktopRuntimeAvailability::Available
                } else if environment.is_development() {
                    dogi_ui::DesktopRuntimeAvailability::Development
                } else {
                    dogi_ui::DesktopRuntimeAvailability::Unmanaged
                },
                detail: runtime_detail,
                manage: Arc::new(move |operation| service::manage(&runtime_environment, operation)),
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
            network,
            updates: crate::update::application_update_manager(
                environment,
                update_store,
                network_service,
            ),
        },
    )
}

fn network_preferences(
    service: crate::network::NetworkService,
) -> dogi_ui::NetworkPreferencesIntegration {
    let fallback = service.default_preferences();
    let (initial, load_error) = match service.load_preferences() {
        Ok(preferences) => (preferences, None),
        Err(error) => (fallback, Some(error.to_string())),
    };
    let save_service = service.clone();
    let integration = dogi_ui::NetworkPreferencesIntegration::new(
        initial,
        move |draft| save_service.save(draft),
        move |draft| service.test(draft),
    );
    match load_error {
        Some(error) => integration.with_load_error(error),
        None => integration,
    }
}

fn application_preferences(
    store: ApplicationConfigStore,
) -> dogi_ui::ApplicationPreferencesIntegration {
    let fallback = store.default_preferences();
    let (initial, load_error) = match store.load_preferences() {
        Ok(preferences) => (preferences, None),
        Err(error) => (fallback, Some(error.to_string())),
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
