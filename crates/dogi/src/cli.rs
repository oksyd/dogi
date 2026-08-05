use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode};
use std::time::Duration;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use dogi_core::{
    ActiveApplication, AppProfile, ButtonAction, CapabilityState, DeviceInfo, DogiError,
    HidppFeature, LOGITECH_VENDOR_ID, LocalRuntimePlan, Master3sButton, Master3sRuntimeEvent,
    Master3sSettings, ResolvedRuntimeAction, Result, RuntimeActionSource, SettingsApplyPlan,
    SettingsApplyReport, SettingsApplyStatus, ThumbWheelMode, ThumbWheelRuntimeAction,
    WheelRatchetMode, build_master3s_apply_plan, build_master3s_device_diff_plan,
    build_master3s_runtime_plan, device_settings_id, effective_master3s_settings_for_app,
    resolve_master3s_runtime_event, resolved_logitech_device_name, settings_apply_step_scope,
};

use crate::application;
use crate::device::DeviceService;
use crate::runtime::UINPUT_PATH;
use crate::runtime::{
    RuntimeActionExecution, SystemRuntimeActionExecutor, execute_runtime_actions_with,
};
use crate::runtime::{control as runtime_control, service as runtime_service};

#[cfg(test)]
use crate::runtime::actions::RuntimeActionExecutionStatus;

const LINUX_UDEV_RULE_PATH: &str = "/etc/udev/rules.d/70-dogi-logitech.rules";
const LINUX_UDEV_RULE: &str = include_str!("../assets/linux/70-dogi-logitech.rules");
const LINUX_UDEV_RELOAD_HINT: &str = "sudo udevadm control --reload-rules && sudo udevadm trigger --subsystem-match=hidraw && sudo udevadm trigger --subsystem-match=misc";
const LINUX_UINPUT_HINT: &str = "Dogi custom actions need write access to /dev/uinput";
const PREVIEW_DIVERSION_SPEED_PERCENT: u16 = 101;

#[derive(Debug, Parser)]
#[command(name = "dogi")]
#[command(version)]
#[command(about = "Lightweight Logitech mouse configuration")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Scan supported Logitech HID devices.
    List(ListArgs),
    /// Show complete metadata for one scanned device.
    Inspect(InspectArgs),
    /// Run local diagnostics for HID access.
    Doctor(DoctorArgs),
    /// Manage the local Master 3S profile.
    Config(ConfigArgs),
    /// Inspect or dry-run local software actions.
    Runtime(RuntimeArgs),
    /// Manage local desktop service integration.
    Service(ServiceArgs),
    /// Manage Linux udev rules for hidraw and uinput access.
    Udev(UdevArgs),
    /// Launch the Slint desktop UI.
    Gui,
}

#[derive(Debug, Args)]
struct ListArgs {
    /// Include non-Logitech HID devices.
    #[arg(long)]
    all: bool,
    /// Print JSON instead of a table.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct InspectArgs {
    /// Device id from `dogi list`.
    id: String,
    /// Print JSON instead of a readable report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct DoctorArgs {
    /// Print JSON instead of a readable report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, Args)]
struct RuntimeArgs {
    #[command(subcommand)]
    command: RuntimeCommand,
}

#[derive(Debug, Subcommand)]
enum RuntimeCommand {
    /// Print the local runtime plan for the saved profile.
    Plan(RuntimePlanArgs),
    /// Listen for HID++ runtime events, dry-run by default.
    Listen(RuntimeListenArgs),
    /// Run the local runtime service loop.
    Run(RuntimeRunArgs),
}

#[derive(Debug, Args)]
struct RuntimePlanArgs {
    /// Print JSON instead of a readable report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RuntimeListenArgs {
    /// Device id from `dogi list`. If omitted, dogi auto-selects the only paired HID++ mouse.
    #[arg(long)]
    device_id: Option<String>,
    /// Number of parsed runtime events to wait for.
    #[arg(long, default_value_t = 1)]
    events: usize,
    /// Stop if no matching runtime event arrives within this many milliseconds.
    #[arg(long, default_value_t = 30_000)]
    idle_timeout_ms: u64,
    /// Execute resolved actions through the local system input backend.
    #[arg(long)]
    execute_actions: bool,
    /// Print JSON instead of readable events.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RuntimeRunArgs {
    /// Device id from `dogi list`. If omitted, dogi auto-selects the only paired HID++ mouse.
    #[arg(long)]
    device_id: Option<String>,
    /// Stop after this many parsed runtime events. Omit for a long-running service.
    #[arg(long)]
    max_events: Option<usize>,
    /// Stop if no matching runtime event arrives within this many milliseconds before polling again.
    #[arg(long, default_value_t = 30_000)]
    idle_timeout_ms: u64,
    /// Execute resolved actions through the local system input backend.
    #[arg(long)]
    execute_actions: bool,
    /// Allow app-profile transitions to apply effective settings to the Logitech device.
    #[arg(long)]
    allow_device_write: bool,
}

#[derive(Debug, Args)]
struct ServiceArgs {
    #[command(subcommand)]
    command: ServiceCommand,
}

#[derive(Debug, Args)]
struct UdevArgs {
    #[command(subcommand)]
    command: UdevCommand,
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    /// Print Dogi's generated systemd user unit.
    Print,
    /// Install, enable, and start the Dogi desktop runtime.
    Install,
    /// Stop and remove the Dogi desktop runtime.
    Uninstall,
    /// Print the generated unit path.
    Path,
}

#[derive(Debug, Subcommand)]
enum UdevCommand {
    /// Print the udev rules for hidraw and uinput access.
    Print,
    /// Install the udev rules. Usually run the resulting binary with sudo.
    Install,
    /// Remove the installed udev rules. Usually run the resulting binary with sudo.
    Uninstall,
    /// Print the udev rules path.
    Path,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the saved profile, or defaults if none exists.
    Show(ConfigOutputArgs),
    /// Print the config file path.
    Path,
    /// Update and save the local profile.
    Set(ConfigSetArgs),
    /// Manage app-specific local profiles.
    AppProfile(ConfigAppProfileArgs),
    /// Reset the local profile to defaults.
    Reset(ConfigOutputArgs),
    /// Print the HID++ apply plan for the saved profile.
    Plan(ConfigPlanArgs),
    /// Apply the saved profile to a mouse after explicit write approval.
    Apply(ConfigApplyArgs),
}

#[derive(Debug, Args)]
struct ConfigAppProfileArgs {
    #[command(subcommand)]
    command: ConfigAppProfileCommand,
}

#[derive(Debug, Subcommand)]
enum ConfigAppProfileCommand {
    /// List saved app-specific profiles.
    List(ConfigOutputArgs),
    /// Add or update an app-specific profile.
    Set(ConfigAppProfileSetArgs),
    /// Remove an app-specific profile.
    Remove(ConfigAppProfileRemoveArgs),
}

#[derive(Debug, Args)]
struct ConfigAppProfileSetArgs {
    /// Application name/class/executable substring, such as firefox or code.
    #[arg(long)]
    app: String,
    /// Pointer speed percent for this app, clamped to 50..200.
    #[arg(long)]
    pointer_speed: Option<u8>,
    /// Thumb wheel behavior for this app.
    #[arg(long)]
    thumb_wheel: Option<CliThumbWheelMode>,
    /// Print JSON instead of a readable report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ConfigAppProfileRemoveArgs {
    /// Application name/class/executable substring to remove.
    #[arg(long)]
    app: String,
    /// Print JSON instead of a readable report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ConfigOutputArgs {
    /// Print JSON instead of a readable report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ConfigPlanArgs {
    /// Device id to include in the plan.
    #[arg(long, default_value = "saved-profile")]
    device_id: String,
    /// Print JSON instead of a readable report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ConfigApplyArgs {
    /// Device id from `dogi list`.
    #[arg(long)]
    device_id: String,
    /// Required guard for HID++ configuration writes.
    #[arg(long)]
    allow_device_write: bool,
    /// Print JSON instead of a readable report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ConfigSetArgs {
    /// Profile name.
    #[arg(long)]
    profile_name: Option<String>,
    /// Pointer speed percent, clamped to 50..200.
    #[arg(long)]
    pointer_speed: Option<u8>,
    /// Enable SmartShift.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "no_smart_shift")]
    smart_shift: bool,
    /// Disable SmartShift.
    #[arg(long, action = ArgAction::SetTrue)]
    no_smart_shift: bool,
    /// SmartShift sensitivity, clamped to 1..50.
    #[arg(long)]
    smart_shift_threshold: Option<u8>,
    /// Wheel mode.
    #[arg(long)]
    wheel_mode: Option<CliWheelMode>,
    /// Enable smooth scrolling.
    #[arg(
        long = "smooth-scrolling",
        visible_alias = "high-resolution-scroll",
        action = ArgAction::SetTrue,
        conflicts_with = "no_high_resolution_scroll"
    )]
    high_resolution_scroll: bool,
    /// Disable smooth scrolling.
    #[arg(
        long = "no-smooth-scrolling",
        visible_alias = "no-high-resolution-scroll",
        action = ArgAction::SetTrue
    )]
    no_high_resolution_scroll: bool,
    /// Use the natural scroll direction.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "no_natural_scroll")]
    natural_scroll: bool,
    /// Use the standard scroll direction.
    #[arg(long, action = ArgAction::SetTrue)]
    no_natural_scroll: bool,
    /// Thumb wheel behavior.
    #[arg(long)]
    thumb_wheel: Option<CliThumbWheelMode>,
    /// Horizontal thumb wheel speed percent, clamped to 25..400.
    #[arg(long)]
    thumb_wheel_speed: Option<u16>,
    /// Button mapping such as gesture=overview or back=default. Can be repeated.
    #[arg(long = "button", value_parser = parse_button_mapping)]
    button_mappings: Vec<CliButtonMapping>,
    /// Print JSON instead of a readable report.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliWheelMode {
    Ratchet,
    FreeSpin,
    SmartShift,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliThumbWheelMode {
    HorizontalScroll,
    TabSwitch,
    Zoom,
    Volume,
    Disabled,
}

#[derive(Clone, Copy, Debug)]
struct CliButtonMapping {
    button: Master3sButton,
    action: ButtonAction,
}

pub fn run() -> ExitCode {
    match Cli::try_parse() {
        Ok(cli) => match execute(cli) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            let code = error.exit_code();
            let _ = error.print();
            ExitCode::from(code as u8)
        }
    }
}

fn execute(cli: Cli) -> Result<()> {
    match command_or_default(cli) {
        Command::List(args) => list_devices(args),
        Command::Inspect(args) => inspect_device(args),
        Command::Doctor(args) => doctor(args),
        Command::Config(args) => config(args),
        Command::Runtime(args) => runtime(args),
        Command::Service(args) => service(args),
        Command::Udev(args) => udev(args),
        Command::Gui => application::launch_gui(),
    }
}

fn command_or_default(cli: Cli) -> Command {
    cli.command.unwrap_or(Command::Gui)
}

fn list_devices(args: ListArgs) -> Result<()> {
    let daemon = DeviceService::new();
    let devices = if args.all {
        daemon.scan_all_devices()?
    } else {
        daemon.scan_devices()?
    };

    if args.json {
        print_json(&devices)
    } else {
        print_device_table(&devices, args.all);
        Ok(())
    }
}

fn inspect_device(args: InspectArgs) -> Result<()> {
    let daemon = DeviceService::new();
    let device = daemon.find_device(&args.id)?;

    if args.json {
        print_json(&device)
    } else {
        print_device_report(&device);
        Ok(())
    }
}

fn doctor(args: DoctorArgs) -> Result<()> {
    let daemon = DeviceService::new();
    let all_devices = daemon.scan_all_devices()?;
    let report = doctor_report_from_devices(&all_devices);

    if args.json {
        print_json(&report)
    } else {
        print_doctor_report(&report);
        Ok(())
    }
}

fn doctor_report_from_devices(all_devices: &[DeviceInfo]) -> DoctorReport {
    doctor_report_from_devices_with_environment(
        all_devices,
        runtime_uinput_status(),
        active_window_status(),
        systemd_user_service_status(),
    )
}

fn doctor_report_from_devices_with_environment(
    all_devices: &[DeviceInfo],
    uinput: RuntimeEnvironmentStatus,
    active_window: RuntimeEnvironmentStatus,
    service: SystemdUserServiceStatus,
) -> DoctorReport {
    let logitech_devices = all_devices
        .iter()
        .filter(|device| device.vendor_id == LOGITECH_VENDOR_ID)
        .count();
    let hidpp_interfaces = all_devices
        .iter()
        .filter(|device| device.capabilities.hidpp == CapabilityState::Supported)
        .count();
    let hidraw_readable = all_devices
        .iter()
        .filter(|device| device.access.hidraw_readable)
        .count();
    let hidraw_readwrite = all_devices
        .iter()
        .filter(|device| device.access.hidraw_readwrite)
        .count();

    DoctorReport {
        hid_backend: "linux-sysfs-hidraw",
        device_writer_status: "explicit apply and configured app-profile transitions",
        total_hid_devices: all_devices.len(),
        logitech_devices,
        hidpp_interfaces,
        hidraw_readable,
        hidraw_readwrite,
        linux_permission_hint: cfg!(target_os = "linux").then_some(
            "HID++ discovery, explicit apply, and app-profile transitions need read/write access to the HID++ endpoint",
        ),
        linux_udev_rule_path: cfg!(target_os = "linux").then_some(LINUX_UDEV_RULE_PATH),
        linux_udev_rule: cfg!(target_os = "linux").then_some(LINUX_UDEV_RULE),
        linux_udev_reload_hint: cfg!(target_os = "linux").then_some(LINUX_UDEV_RELOAD_HINT),
        runtime_uinput_path: cfg!(target_os = "linux").then_some(UINPUT_PATH),
        runtime_uinput_writable: uinput.available,
        runtime_uinput_detail: uinput.detail,
        runtime_uinput_hint: cfg!(target_os = "linux").then_some(LINUX_UINPUT_HINT),
        active_window_backend: cfg!(target_os = "linux").then_some("xprop"),
        active_window_available: active_window.available,
        active_window_detail: active_window.detail,
        systemd_user_service_path: service.path,
        systemd_user_service_installed: service.installed,
    }
}

fn print_doctor_report(report: &DoctorReport) {
    println!("dogi doctor");
    println!("  HID backend: {}", report.hid_backend);
    println!("  Device writer: {}", report.device_writer_status);
    println!("  HID devices: {}", report.total_hid_devices);
    println!("  Logitech devices: {}", report.logitech_devices);
    println!("  HID++ interfaces: {}", report.hidpp_interfaces);
    println!("  Readable hidraw nodes: {}", report.hidraw_readable);
    println!("  Read/write hidraw nodes: {}", report.hidraw_readwrite);

    if let Some(hint) = report.linux_permission_hint {
        println!("  Linux permissions: {hint}");
    }

    if let (Some(path), Some(rule), Some(reload_hint)) = (
        report.linux_udev_rule_path,
        report.linux_udev_rule,
        report.linux_udev_reload_hint,
    ) {
        println!("  udev rule path: {path}");
        println!("  udev rule: {rule}");
        println!("  reload rules: {reload_hint}");
    }

    if let Some(path) = report.runtime_uinput_path {
        println!(
            "  uinput: {} ({})",
            if report.runtime_uinput_writable {
                "writable"
            } else {
                "not writable"
            },
            path
        );
        println!("  uinput detail: {}", report.runtime_uinput_detail);
    }

    if let Some(hint) = report.runtime_uinput_hint {
        println!("  runtime permissions: {hint}");
    }

    if let Some(backend) = report.active_window_backend {
        println!(
            "  active-window backend: {} ({})",
            backend,
            if report.active_window_available {
                "available"
            } else {
                "unavailable"
            }
        );
        println!("  active-window detail: {}", report.active_window_detail);
    }

    if let Some(path) = &report.systemd_user_service_path {
        println!("  systemd user service: {path}");
        println!(
            "  service installed: {}",
            report.systemd_user_service_installed
        );
    }
}

fn config(args: ConfigArgs) -> Result<()> {
    match args.command {
        ConfigCommand::Show(args) => config_show(args),
        ConfigCommand::Path => config_path(),
        ConfigCommand::Set(args) => config_set(args),
        ConfigCommand::AppProfile(args) => config_app_profile(args),
        ConfigCommand::Reset(args) => config_reset(args),
        ConfigCommand::Plan(args) => config_plan(args),
        ConfigCommand::Apply(args) => config_apply(args),
    }
}

fn runtime(args: RuntimeArgs) -> Result<()> {
    match args.command {
        RuntimeCommand::Plan(args) => runtime_plan(args),
        RuntimeCommand::Listen(args) => runtime_listen(args),
        RuntimeCommand::Run(args) => runtime_run(args),
    }
}

fn service(args: ServiceArgs) -> Result<()> {
    match args.command {
        ServiceCommand::Print => runtime_service::print_unit(),
        ServiceCommand::Install => runtime_service::install(),
        ServiceCommand::Uninstall => runtime_service::uninstall(),
        ServiceCommand::Path => {
            println!("{}", runtime_service::path()?.display());
            Ok(())
        }
    }
}

fn udev(args: UdevArgs) -> Result<()> {
    match args.command {
        UdevCommand::Print => udev_print(),
        UdevCommand::Install => udev_install(),
        UdevCommand::Uninstall => udev_uninstall(),
        UdevCommand::Path => udev_path(),
    }
}

fn config_show(args: ConfigOutputArgs) -> Result<()> {
    let daemon = DeviceService::new();
    let settings = daemon.load_master3s_settings()?;
    let report = ConfigReport::new(daemon.master3s_settings_path()?, settings);

    if args.json {
        print_json(&report)
    } else {
        print_config_report(&report);
        Ok(())
    }
}

fn runtime_plan(args: RuntimePlanArgs) -> Result<()> {
    let daemon = DeviceService::new();
    let settings = daemon.load_master3s_settings()?;
    let plan = daemon.plan_master3s_runtime(&settings);

    if args.json {
        print_json(&plan)
    } else {
        print_runtime_plan(&plan);
        Ok(())
    }
}

fn runtime_listen(args: RuntimeListenArgs) -> Result<()> {
    let daemon = DeviceService::new();
    let device_id = resolve_runtime_device_id(&daemon, args.device_id.as_deref())?;
    let settings_id = resolve_device_settings_id(&daemon, &device_id);
    let settings = daemon.load_master3s_settings_for_device(&settings_id)?;
    let runtime_plan = daemon.plan_master3s_runtime(&settings);
    let events = daemon.listen_master3s_runtime_events(
        &device_id,
        args.events,
        Duration::from_millis(args.idle_timeout_ms),
    )?;
    let actions = events
        .iter()
        .flat_map(|event| resolve_master3s_runtime_event(&runtime_plan, event))
        .collect::<Vec<_>>();
    let executions = if args.execute_actions {
        daemon.execute_master3s_runtime_actions(&actions)?
    } else {
        Vec::new()
    };
    let report = RuntimeListenReport {
        device_id,
        runtime_plan,
        events,
        actions,
        executions,
        execute_actions: args.execute_actions,
    };

    if args.json {
        print_json(&report)
    } else {
        print_runtime_listen_report(&report);
        Ok(())
    }
}

fn runtime_run(args: RuntimeRunArgs) -> Result<()> {
    let preview_state = runtime_control::RuntimePreviewState::start()?;
    if args.max_events.is_some() {
        loop {
            match runtime_run_session(&args, &preview_state)? {
                RuntimeSessionOutcome::Completed => return Ok(()),
                RuntimeSessionOutcome::SwitchDevice => continue,
            }
        }
    }

    let mut previous_error = String::new();
    let mut repeated_failures = 0_u32;
    loop {
        match runtime_run_session(&args, &preview_state) {
            Ok(RuntimeSessionOutcome::Completed) => return Ok(()),
            Ok(RuntimeSessionOutcome::SwitchDevice) => continue,
            Err(error) => {
                let detail = error.to_string();
                preview_state.fail_pending(detail.clone());
                if detail == previous_error {
                    repeated_failures = repeated_failures.saturating_add(1);
                    if repeated_failures.is_multiple_of(20) {
                        eprintln!(
                            "Dogi runtime is still waiting to reconnect ({repeated_failures} attempts): {detail}"
                        );
                    }
                } else {
                    eprintln!("Dogi runtime is waiting to reconnect: {detail}");
                    previous_error = detail;
                    repeated_failures = 1;
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeSessionOutcome {
    Completed,
    SwitchDevice,
}

fn runtime_run_session(
    args: &RuntimeRunArgs,
    preview_state: &runtime_control::RuntimePreviewState,
) -> Result<RuntimeSessionOutcome> {
    let daemon = DeviceService::new();
    let preview_device_id = preview_state
        .snapshot()
        .preview
        .map(|preview| preview.device_id);
    let requested_device_id = args.device_id.as_deref().or(preview_device_id.as_deref());
    let device_id = resolve_runtime_device_id(&daemon, requested_device_id)?;
    let settings_id = resolve_device_settings_id(&daemon, &device_id);
    let mut base_settings = daemon.load_master3s_settings_for_device(&settings_id)?;
    let idle_timeout = Duration::from_millis(args.idle_timeout_ms);
    let mut processed_events = 0_usize;
    let mut focus_warning_printed = false;
    let mut settings_warning_printed = false;
    let mut active_effective_state = RuntimeEffectiveState::default();
    let mut preview_error: Option<(u64, String)> = None;
    let mut listener = daemon.open_master3s_runtime_event_listener(&device_id)?;
    let mut action_executor = None;

    println!("Runtime service for {device_id}");
    println!(
        "  mode: actions {}, device writes {}",
        if args.execute_actions {
            "enabled"
        } else {
            "dry-run"
        },
        if args.allow_device_write {
            "enabled"
        } else {
            "disabled"
        }
    );

    if args.max_events == Some(0) {
        return Ok(RuntimeSessionOutcome::Completed);
    }

    loop {
        if reload_runtime_base_settings(
            &daemon,
            &settings_id,
            &mut base_settings,
            &mut settings_warning_printed,
        ) {
            println!("  settings reloaded");
        }

        let active_application = runtime_active_application(&daemon, &mut focus_warning_printed);
        let effective =
            effective_master3s_settings_for_app(&base_settings, active_application.as_ref());
        let matched_profile_name = effective
            .matched_profile
            .as_ref()
            .map(|profile| profile.app_name.clone());
        let preview_snapshot = preview_state.snapshot();
        if preview_error
            .as_ref()
            .is_some_and(|(generation, _)| *generation != preview_snapshot.generation)
        {
            preview_error = None;
        }
        let active_preview = preview_snapshot
            .preview
            .as_ref()
            .filter(|preview| preview.device_id == device_id);
        if let Some(preview) = preview_snapshot
            .preview
            .as_ref()
            .filter(|preview| preview.device_id != device_id)
        {
            if args.device_id.is_none() {
                return Ok(RuntimeSessionOutcome::SwitchDevice);
            }
            preview_state.publish_failed(
                preview_snapshot.generation,
                format!(
                    "preview device {} is not the active runtime device {}",
                    preview.device_id, device_id
                ),
            );
        }

        let device_settings = runtime_device_settings(&effective.settings, active_preview);
        let profile_changed =
            active_effective_state.profile_name.as_deref() != matched_profile_name.as_deref();
        let preview_transition = active_effective_state.preview_active != active_preview.is_some();
        let mut preview_apply_failed = false;
        let device_apply_needed = profile_changed || preview_transition;
        if device_apply_needed {
            if profile_changed {
                print_runtime_profile_change(
                    active_application.as_ref(),
                    matched_profile_name.as_deref(),
                );
            }
            let plan = runtime_device_apply_plan(
                &device_id,
                active_effective_state.settings.as_ref(),
                &device_settings,
                preview_transition,
            );
            if plan.steps.is_empty() {
                // Adopting the current profile at startup must not mutate the mouse. Device
                // writes begin only after an observed profile or preview transition.
            } else if args.allow_device_write {
                let report =
                    daemon.apply_master3s_settings_plan(&device_id, &device_settings, &plan)?;
                let failed_steps = count_failed_apply_steps(&report);
                println!(
                    "  device apply: {} failed step{}",
                    failed_steps,
                    plural(failed_steps)
                );
                if (active_preview.is_some() || preview_transition)
                    && let Some(detail) = preview_thumb_wheel_failure(&report)
                {
                    preview_state.publish_failed(preview_snapshot.generation, detail.clone());
                    preview_error = Some((preview_snapshot.generation, detail));
                    preview_apply_failed = true;
                }
            } else {
                println!("  device apply: skipped, --allow-device-write not set");
                if active_preview.is_some() || preview_transition {
                    preview_state.publish_failed(
                        preview_snapshot.generation,
                        "horizontal scroll preview needs device-write access",
                    );
                    preview_error = Some((
                        preview_snapshot.generation,
                        "horizontal scroll preview needs device-write access".to_owned(),
                    ));
                    preview_apply_failed = true;
                }
            }
        }
        if active_effective_state.needs_update(
            matched_profile_name.as_deref(),
            &device_settings,
            active_preview.is_some(),
        ) {
            active_effective_state.update(
                matched_profile_name.clone(),
                &device_settings,
                active_preview.is_some(),
            );
        }

        let mut runtime_plan = daemon.plan_master3s_runtime(&effective.settings);
        if let Some(preview) = active_preview {
            runtime_plan.thumb_wheel = Some(ThumbWheelRuntimeAction::HorizontalScroll {
                speed_percent: preview.speed_percent,
            });
        }
        if !preview_apply_failed && (preview_snapshot.preview.is_none() || active_preview.is_some())
        {
            if let Some((_, detail)) = &preview_error {
                preview_state.publish_failed(preview_snapshot.generation, detail.clone());
            } else {
                preview_state.publish_applied(preview_snapshot.generation);
            }
        }
        let events = listener.read_events(1, idle_timeout)?;
        if events.is_empty() {
            continue;
        }

        for event in events {
            let actions = resolve_master3s_runtime_event(&runtime_plan, &event);
            let executions = if !args.execute_actions {
                Vec::new()
            } else if let Some(executor) = action_executor.as_mut() {
                execute_runtime_actions_with(&actions, executor)
            } else if actions.iter().any(|action| {
                !matches!(
                    &action.command,
                    dogi_core::RuntimeCommand::Noop | dogi_core::RuntimeCommand::Unsupported
                )
            }) {
                let executor = action_executor.insert(SystemRuntimeActionExecutor::open()?);
                execute_runtime_actions_with(&actions, executor)
            } else {
                daemon.execute_master3s_runtime_actions(&actions)?
            };

            print_runtime_loop_event(&event, &actions, &executions, args.execute_actions);
            processed_events += 1;

            if args
                .max_events
                .is_some_and(|max_events| processed_events >= max_events)
            {
                println!("Runtime service stopped after {processed_events} event(s)");
                return Ok(RuntimeSessionOutcome::Completed);
            }
        }
    }
}

fn reload_runtime_base_settings(
    daemon: &DeviceService,
    settings_id: &str,
    current: &mut Master3sSettings,
    warning_printed: &mut bool,
) -> bool {
    match daemon.load_master3s_settings_for_device(settings_id) {
        Ok(settings) => {
            *warning_printed = false;
            if &settings != current {
                *current = settings;
                true
            } else {
                false
            }
        }
        Err(error) => {
            if !*warning_printed {
                eprintln!("settings reload failed; keeping previous settings: {error}");
                *warning_printed = true;
            }
            false
        }
    }
}

#[derive(Debug, Default)]
struct RuntimeEffectiveState {
    profile_name: Option<String>,
    settings: Option<Master3sSettings>,
    preview_active: bool,
}

impl RuntimeEffectiveState {
    fn needs_update(
        &self,
        profile_name: Option<&str>,
        settings: &Master3sSettings,
        preview_active: bool,
    ) -> bool {
        self.profile_name.as_deref() != profile_name
            || self.settings_changed(settings)
            || self.preview_active != preview_active
    }

    fn settings_changed(&self, settings: &Master3sSettings) -> bool {
        self.settings.as_ref() != Some(settings)
    }

    fn update(
        &mut self,
        profile_name: Option<String>,
        settings: &Master3sSettings,
        preview_active: bool,
    ) {
        self.profile_name = profile_name;
        self.settings = Some(settings.clone());
        self.preview_active = preview_active;
    }
}

fn runtime_device_settings(
    effective: &Master3sSettings,
    preview: Option<&runtime_control::HorizontalScrollPreview>,
) -> Master3sSettings {
    let mut settings = effective.clone();
    if preview.is_some() {
        settings.thumb_wheel = ThumbWheelMode::HorizontalScroll;
        settings.thumb_wheel_speed_percent = PREVIEW_DIVERSION_SPEED_PERCENT;
    }
    settings.normalized()
}

fn runtime_device_apply_plan(
    device_id: &str,
    baseline: Option<&Master3sSettings>,
    target: &Master3sSettings,
    force_thumb_wheel: bool,
) -> SettingsApplyPlan {
    // A fresh runtime session has no trustworthy knowledge of the mouse's current state.
    // Treat the target as its own baseline so startup and reconnect stay read-only.
    let mut plan = build_master3s_device_diff_plan(device_id, baseline.unwrap_or(target), target);
    if force_thumb_wheel
        && !plan
            .steps
            .iter()
            .any(|step| step.feature == HidppFeature::ThumbWheel)
        && let Some(step) = build_master3s_apply_plan(device_id, target)
            .steps
            .into_iter()
            .find(|step| step.feature == HidppFeature::ThumbWheel)
    {
        plan.steps.push(step);
    }
    plan
}

fn preview_thumb_wheel_failure(report: &SettingsApplyReport) -> Option<String> {
    report.outcomes.iter().find_map(|outcome| {
        (outcome.feature == HidppFeature::ThumbWheel
            && matches!(
                outcome.status,
                SettingsApplyStatus::Failed | SettingsApplyStatus::Unsupported
            ))
        .then(|| {
            outcome
                .detail
                .clone()
                .unwrap_or_else(|| outcome.title.clone())
        })
    })
}

fn udev_print() -> Result<()> {
    print!("{LINUX_UDEV_RULE}");
    Ok(())
}

fn udev_install() -> Result<()> {
    write_udev_rule_file(Path::new(LINUX_UDEV_RULE_PATH), LINUX_UDEV_RULE)?;
    println!("installed {LINUX_UDEV_RULE_PATH}");
    println!("reload: {LINUX_UDEV_RELOAD_HINT}");
    println!("reconnect the receiver or mouse if permissions do not update immediately");
    Ok(())
}

fn udev_uninstall() -> Result<()> {
    match fs::remove_file(LINUX_UDEV_RULE_PATH) {
        Ok(()) => {
            println!("removed {LINUX_UDEV_RULE_PATH}");
            println!("reload: {LINUX_UDEV_RELOAD_HINT}");
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            println!("udev rule not installed: {LINUX_UDEV_RULE_PATH}");
            Ok(())
        }
        Err(error) => Err(DogiError::Config(format!(
            "failed to remove {LINUX_UDEV_RULE_PATH}: {error}"
        ))),
    }
}

fn udev_path() -> Result<()> {
    println!("{LINUX_UDEV_RULE_PATH}");
    Ok(())
}

fn resolve_runtime_device_id(daemon: &DeviceService, requested: Option<&str>) -> Result<String> {
    if let Some(requested) = requested {
        let requested = requested.trim();
        if requested.is_empty() {
            return Err(DogiError::InvalidArgument(
                "--device-id cannot be empty".to_owned(),
            ));
        }
        return Ok(requested.to_owned());
    }

    let devices = daemon.scan_devices()?;
    select_runtime_device_id(&devices)
}

fn resolve_device_settings_id(daemon: &DeviceService, device_id: &str) -> String {
    daemon
        .find_device(device_id)
        .map(|device| device_settings_id(&device))
        .unwrap_or_else(|_| device_id.to_owned())
}

fn select_runtime_device_id(devices: &[DeviceInfo]) -> Result<String> {
    let candidates = runtime_device_candidates(devices);

    match candidates.as_slice() {
        [device] => Ok(device.id.clone()),
        [] => Err(DogiError::DeviceNotFound),
        _ => Err(DogiError::InvalidArgument(format!(
            "multiple paired HID++ Logitech devices found; pass --device-id explicitly: {}",
            candidates
                .iter()
                .map(|device| format!("{} ({})", device.id, display_device_name(device)))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn runtime_device_candidates(devices: &[DeviceInfo]) -> Vec<&DeviceInfo> {
    devices
        .iter()
        .filter(|device| {
            device.is_logitech()
                && device.paired_device.is_some()
                && device.report_descriptor.is_hidpp_interface()
        })
        .collect()
}

fn systemd_user_service_path() -> Result<PathBuf> {
    runtime_service::path()
}

fn write_udev_rule_file(path: &Path, contents: &str) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        DogiError::Config(format!("udev rule path has no parent: {}", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        DogiError::Config(format!("failed to create {}: {error}", parent.display()))
    })?;

    let tmp_path = path.with_extension("rules.tmp");
    fs::write(&tmp_path, contents).map_err(|error| {
        DogiError::Config(format!("failed to write {}: {error}", tmp_path.display()))
    })?;
    fs::rename(&tmp_path, path).map_err(|error| {
        DogiError::Config(format!(
            "failed to replace {} with {}: {error}",
            path.display(),
            tmp_path.display()
        ))
    })
}

#[derive(Clone, Debug)]
struct RuntimeEnvironmentStatus {
    available: bool,
    detail: String,
}

#[derive(Clone, Debug)]
struct SystemdUserServiceStatus {
    path: Option<String>,
    installed: bool,
}

fn runtime_uinput_status() -> RuntimeEnvironmentStatus {
    #[cfg(target_os = "linux")]
    {
        match fs::OpenOptions::new().write(true).open(UINPUT_PATH) {
            Ok(_) => RuntimeEnvironmentStatus {
                available: true,
                detail: "opened for writing".to_owned(),
            },
            Err(error) => RuntimeEnvironmentStatus {
                available: false,
                detail: format!("failed to open {UINPUT_PATH} for writing: {error}"),
            },
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        RuntimeEnvironmentStatus {
            available: false,
            detail: "uinput runtime actions are only implemented on Linux".to_owned(),
        }
    }
}

fn active_window_status() -> RuntimeEnvironmentStatus {
    #[cfg(target_os = "linux")]
    {
        match ProcessCommand::new("xprop").arg("-version").output() {
            Ok(output) if output.status.success() => RuntimeEnvironmentStatus {
                available: true,
                detail: "xprop is available".to_owned(),
            },
            Ok(output) => RuntimeEnvironmentStatus {
                available: false,
                detail: format!(
                    "xprop returned non-zero status: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            },
            Err(error) => RuntimeEnvironmentStatus {
                available: false,
                detail: format!("xprop is not available: {error}"),
            },
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        RuntimeEnvironmentStatus {
            available: false,
            detail: "active-window detection is only implemented for Linux X11".to_owned(),
        }
    }
}

fn systemd_user_service_status() -> SystemdUserServiceStatus {
    match systemd_user_service_path() {
        Ok(path) => SystemdUserServiceStatus {
            installed: path.exists(),
            path: Some(path.display().to_string()),
        },
        Err(error) => SystemdUserServiceStatus {
            installed: false,
            path: Some(format!("unavailable: {error}")),
        },
    }
}

fn config_path() -> Result<()> {
    let daemon = DeviceService::new();
    println!("{}", daemon.master3s_settings_path()?.display());
    Ok(())
}

fn config_set(args: ConfigSetArgs) -> Result<()> {
    let daemon = DeviceService::new();
    let mut settings = daemon.load_master3s_settings()?;
    apply_config_set_args(&mut settings, &args);
    let path = daemon.save_master3s_settings(&settings)?;
    let report = ConfigReport::new(path, settings.normalized());

    if args.json {
        print_json(&report)
    } else {
        print_config_report(&report);
        Ok(())
    }
}

fn config_app_profile(args: ConfigAppProfileArgs) -> Result<()> {
    match args.command {
        ConfigAppProfileCommand::List(args) => config_app_profile_list(args),
        ConfigAppProfileCommand::Set(args) => config_app_profile_set(args),
        ConfigAppProfileCommand::Remove(args) => config_app_profile_remove(args),
    }
}

fn config_app_profile_list(args: ConfigOutputArgs) -> Result<()> {
    let daemon = DeviceService::new();
    let settings = daemon.load_master3s_settings()?;
    let report = ConfigReport::new(daemon.master3s_settings_path()?, settings);

    if args.json {
        print_json(&report.settings.app_profiles)
    } else {
        print_app_profiles(&report.settings.app_profiles);
        Ok(())
    }
}

fn config_app_profile_set(args: ConfigAppProfileSetArgs) -> Result<()> {
    let daemon = DeviceService::new();
    let mut settings = daemon.load_master3s_settings()?;
    upsert_app_profile(&mut settings, &args)?;
    let path = daemon.save_master3s_settings(&settings)?;
    let report = ConfigReport::new(path, settings);

    if args.json {
        print_json(&report)
    } else {
        print_config_report(&report);
        Ok(())
    }
}

fn config_app_profile_remove(args: ConfigAppProfileRemoveArgs) -> Result<()> {
    let daemon = DeviceService::new();
    let mut settings = daemon.load_master3s_settings()?;
    remove_app_profile(&mut settings, &args.app)?;
    let path = daemon.save_master3s_settings(&settings)?;
    let report = ConfigReport::new(path, settings);

    if args.json {
        print_json(&report)
    } else {
        print_config_report(&report);
        Ok(())
    }
}

fn config_reset(args: ConfigOutputArgs) -> Result<()> {
    let daemon = DeviceService::new();
    let path = daemon.reset_master3s_settings()?;
    let settings = daemon.load_master3s_settings()?;
    let report = ConfigReport::new(path, settings);

    if args.json {
        print_json(&report)
    } else {
        print_config_report(&report);
        Ok(())
    }
}

fn config_plan(args: ConfigPlanArgs) -> Result<()> {
    let daemon = DeviceService::new();
    let settings_id = resolve_device_settings_id(&daemon, &args.device_id);
    let settings = daemon.load_master3s_settings_for_device(&settings_id)?;
    let plan = daemon.plan_master3s_settings(&args.device_id, &settings);

    if args.json {
        print_json(&plan)
    } else {
        print_apply_plan(&plan, &settings);
        Ok(())
    }
}

fn config_apply(args: ConfigApplyArgs) -> Result<()> {
    if !args.allow_device_write {
        return Err(DogiError::InvalidArgument(
            "config apply requires --allow-device-write".to_owned(),
        ));
    }

    let daemon = DeviceService::new();
    let settings_id = resolve_device_settings_id(&daemon, &args.device_id);
    let settings = daemon.load_master3s_settings_for_device(&settings_id)?;
    let report = daemon.apply_master3s_settings(&args.device_id, &settings)?;

    if args.json {
        print_json(&report)
    } else {
        print_apply_report(&report);
        Ok(())
    }
}

fn print_device_table(devices: &[DeviceInfo], includes_all_hid: bool) {
    if devices.is_empty() {
        if includes_all_hid {
            println!("no HID devices found");
        } else {
            println!("no Logitech HID devices found");
        }
        return;
    }

    println!(
        "{:<27}  {:<28}  {:<9}  {:<11}  {:<7}  PATH",
        "ID", "NAME", "VID:PID", "CONNECTION", "HID++"
    );

    for device in devices {
        println!(
            "{:<27}  {:<28}  {:<9}  {:<11}  {:<7}  {}",
            truncate(&device.id, 27),
            truncate(display_device_name(device), 28),
            device.product_key(),
            device.connection,
            device.capabilities.hidpp,
            device.path
        );
    }
}

fn print_device_report(device: &DeviceInfo) {
    println!("{}", display_device_name(device));
    println!("  id: {}", device.id);
    println!("  endpoint name: {}", device.name);
    println!("  vendor/product: {}", device.product_key());
    println!("  bus: {}", device.bus);

    if let Some(bus_id) = device.bus_id {
        println!("  bus id: 0x{bus_id:04x}");
    }

    println!("  connection: {}", device.connection);

    if let Some(receiver_kind) = device.receiver_kind {
        println!("  receiver: {receiver_kind}");
    }

    if let Some(manufacturer) = &device.manufacturer {
        println!("  manufacturer: {manufacturer}");
    }

    if let Some(serial_number) = &device.serial_number {
        println!("  serial: {serial_number}");
    }

    if let Some(release_number) = device.release_number {
        println!("  release: {release_number}");
    }

    if let Some(interface_number) = device.interface_number {
        println!("  interface: {interface_number}");
    }

    if let Some(usage_page) = device.usage_page {
        println!("  usage page: 0x{usage_page:04x}");
    }

    if let Some(usage) = device.usage {
        println!("  usage: 0x{usage:04x}");
    }

    println!("  path: {}", device.path);
    println!("  sysfs: {}", device.sysfs_path);

    if let Some(physical_path) = &device.physical_path {
        println!("  physical: {physical_path}");
    }

    if let Some(driver) = &device.driver {
        println!("  driver: {driver}");
    }

    if let Some(paired) = &device.paired_device {
        println!("  paired device:");
        println!("    slot: {}", paired.slot);
        if let Some(name) = &paired.name {
            println!("    name: {name}");
        }
        if let Some(kind) = &paired.kind {
            println!("    kind: {kind}");
        }
        if let Some(wpid) = &paired.wpid {
            println!("    WPID: {wpid}");
        }
        if let Some(protocol) = paired.protocol {
            println!("    HID++ protocol: {protocol}");
        }
        if let Some(unit_id) = &paired.unit_id {
            println!("    unit id: {unit_id}");
        }
        if let Some(model_id) = &paired.model_id {
            println!("    model id: {model_id}");
        }
        println!("    features: {}", paired.feature_count);
        println!(
            "    feature list: {}",
            format_hidpp_features(&paired.features)
        );
    }

    println!("  access:");
    println!("    sysfs readable: {}", device.access.sysfs_readable);
    println!("    hidraw readable: {}", device.access.hidraw_readable);
    println!("    hidraw read/write: {}", device.access.hidraw_readwrite);
    println!("    write policy: {}", device.access.write_policy);
    println!("  battery:");
    println!("    level: {}", device.battery.summary());
    println!("    source: {}", device.battery.source);
    println!("  report descriptor:");
    println!("    bytes: {}", device.report_descriptor.byte_len);
    println!("    report ids: {}", format_report_ids(device));
    println!(
        "    input/output/feature: {}/{}/{}",
        device.report_descriptor.has_input_reports,
        device.report_descriptor.has_output_reports,
        device.report_descriptor.has_feature_reports
    );
    println!(
        "    vendor usage pages: {}",
        format_usage_pages(&device.report_descriptor.vendor_usage_pages)
    );
    println!("  capabilities:");
    println!("    HID++: {}", device.capabilities.hidpp);
    println!("    battery: {}", device.capabilities.battery);
    println!("    DPI: {}", device.capabilities.dpi);
    println!("    button mapping: {}", device.capabilities.button_mapping);
    println!(
        "    onboard profiles: {}",
        device.capabilities.onboard_profiles
    );
    println!("    wheel mode: {}", device.capabilities.wheel_mode);
}

fn display_device_name(device: &DeviceInfo) -> &str {
    resolved_logitech_device_name(device).unwrap_or(&device.name)
}

fn format_hidpp_features(features: &[dogi_core::HidppFeatureInfo]) -> String {
    if features.is_empty() {
        return "none".to_owned();
    }

    features
        .iter()
        .map(|feature| format!("{}@{} v{}", feature.name, feature.index, feature.version))
        .collect::<Vec<_>>()
        .join(", ")
}

fn print_config_report(report: &ConfigReport) {
    println!("Master 3S profile");
    println!("  path: {}", report.path);
    println!("  profile: {}", report.settings.profile_name);
    println!(
        "  pointer speed: {}%",
        report.settings.pointer_speed_percent
    );
    println!(
        "  SmartShift: {} at threshold {}",
        enabled_text(report.settings.smart_shift_enabled),
        report.settings.smart_shift_threshold
    );
    println!("  wheel mode: {}", report.settings.ratchet_mode.label());
    println!(
        "  smooth scrolling: {}",
        enabled_text(report.settings.high_resolution_scroll)
    );
    println!(
        "  scroll direction: {}",
        if report.settings.natural_scroll {
            "natural"
        } else {
            "standard"
        }
    );
    println!("  thumb wheel: {}", report.settings.thumb_wheel.label());
    println!(
        "  horizontal thumb wheel speed: {}%",
        report.settings.thumb_wheel_speed_percent
    );
    println!("  buttons:");
    for binding in &report.settings.buttons {
        println!("    {}: {}", binding.button.label(), binding.action.label());
    }
    print_app_profiles(&report.settings.app_profiles);
    println!(
        "  local runtime: {}",
        if report.runtime_plan.requires_listener() {
            report.runtime_plan.summary()
        } else {
            "not required".to_owned()
        }
    );
}

fn print_app_profiles(profiles: &[AppProfile]) {
    println!("  app profiles: {}", profiles.len());
    for profile in profiles {
        println!(
            "    {}: pointer {}%, thumb wheel {}",
            profile.app_name,
            profile.pointer_speed_percent,
            profile.thumb_wheel.label()
        );
    }
}

fn print_runtime_plan(plan: &LocalRuntimePlan) {
    println!("Master 3S local runtime");
    println!("  profile: {}", plan.profile_name);
    println!("  required: {}", plan.requires_listener());
    println!("  summary: {}", plan.summary());

    if let Some(action) = plan.thumb_wheel {
        println!("  thumb wheel: {}", action.label());
    }

    if !plan.buttons.is_empty() {
        println!("  buttons:");
        for binding in &plan.buttons {
            println!(
                "    {} -> {} (control 0x{:04X})",
                binding.button.label(),
                binding.action.label(),
                binding.control_id
            );
        }
    }

    println!("  app profiles: {}", plan.app_profiles.len());
}

fn print_runtime_listen_report(report: &RuntimeListenReport) {
    println!("Runtime listener for {}", report.device_id);
    println!(
        "  mode: {}",
        if report.execute_actions {
            "execute local actions"
        } else {
            "dry-run"
        }
    );
    println!("  plan: {}", report.runtime_plan.summary());
    println!("  events: {}", report.events.len());

    for event in &report.events {
        println!("  - {}", format_runtime_event(event));
    }

    println!("  actions: {}", report.actions.len());
    for action in &report.actions {
        println!("  - {}", format_resolved_runtime_action(action));
    }

    if report.execute_actions {
        println!("  executions: {}", report.executions.len());
        for execution in &report.executions {
            println!("  - {}", format_runtime_action_execution(execution));
        }
    }
}

fn runtime_active_application(
    daemon: &DeviceService,
    focus_warning_printed: &mut bool,
) -> Option<ActiveApplication> {
    match daemon.active_application() {
        Ok(active_application) => active_application,
        Err(error) => {
            if !*focus_warning_printed {
                eprintln!("active app profile detection unavailable: {error}");
                *focus_warning_printed = true;
            }
            None
        }
    }
}

fn print_runtime_profile_change(
    active_application: Option<&ActiveApplication>,
    matched_profile_name: Option<&str>,
) {
    let active_application = active_application
        .map(ActiveApplication::summary)
        .filter(|summary| !summary.is_empty())
        .unwrap_or_else(|| "unknown application".to_owned());
    let profile = matched_profile_name.unwrap_or("default profile");

    println!("  active app: {active_application}");
    println!("  active profile: {profile}");
}

fn print_runtime_loop_event(
    event: &Master3sRuntimeEvent,
    actions: &[ResolvedRuntimeAction],
    executions: &[RuntimeActionExecution],
    execute_actions: bool,
) {
    println!("  event: {}", format_runtime_event(event));
    if actions.is_empty() {
        println!("    actions: none");
    } else {
        println!("    actions:");
        for action in actions {
            println!("      {}", format_resolved_runtime_action(action));
        }
    }

    if execute_actions {
        if executions.is_empty() {
            println!("    executions: none");
        } else {
            println!("    executions:");
            for execution in executions {
                println!("      {}", format_runtime_action_execution(execution));
            }
        }
    }
}

fn format_runtime_event(event: &Master3sRuntimeEvent) -> String {
    match event {
        Master3sRuntimeEvent::ThumbWheel {
            delta,
            phase,
            resolution,
            direction,
        } => {
            let base = match phase {
                Some(phase) => format!("thumb wheel delta {delta}, phase {phase}"),
                None => format!("thumb wheel delta {delta}"),
            };
            format!("{base}, resolution {resolution}, direction {direction}")
        }
        Master3sRuntimeEvent::DivertedButtons {
            buttons,
            unknown_control_ids,
        } => {
            let known = buttons
                .iter()
                .map(|button| button.label())
                .collect::<Vec<_>>()
                .join(", ");
            let unknown = unknown_control_ids
                .iter()
                .map(|control_id| format!("0x{control_id:04X}"))
                .collect::<Vec<_>>()
                .join(", ");

            match (known.is_empty(), unknown.is_empty()) {
                (false, false) => format!("diverted buttons {known}; unknown {unknown}"),
                (false, true) => format!("diverted buttons {known}"),
                (true, false) => format!("diverted buttons unknown {unknown}"),
                (true, true) => "diverted buttons none".to_owned(),
            }
        }
    }
}

fn format_resolved_runtime_action(action: &ResolvedRuntimeAction) -> String {
    format!(
        "{} -> {}",
        format_runtime_action_source(&action.source),
        action.command.label()
    )
}

fn format_runtime_action_source(source: &RuntimeActionSource) -> String {
    match source {
        RuntimeActionSource::ThumbWheel => "thumb wheel".to_owned(),
        RuntimeActionSource::Button(button) => button.label().to_owned(),
        RuntimeActionSource::UnknownControlId(control_id) => {
            format!("unknown control 0x{control_id:04X}")
        }
    }
}

fn format_runtime_action_execution(execution: &RuntimeActionExecution) -> String {
    let mut text = format!(
        "{} [{}]",
        format_resolved_runtime_action(&execution.action),
        execution.status.label()
    );
    if let Some(detail) = &execution.detail {
        text.push_str(": ");
        text.push_str(detail);
    }
    text
}

fn print_apply_plan(plan: &SettingsApplyPlan, settings: &Master3sSettings) {
    println!("Apply plan for {}", plan.device_id);
    println!("  profile: {}", plan.profile_name);
    println!("  requires device write: {}", plan.requires_device_write());

    for step in &plan.steps {
        println!(
            "  - [{}] {}",
            settings_apply_step_scope(step, settings).label(),
            step.title()
        );
    }
}

fn print_apply_report(report: &SettingsApplyReport) {
    println!("Apply report for {}", report.device_id);
    println!("  profile: {}", report.profile_name);
    println!("  failed steps: {}", report.has_failed_steps());

    for outcome in &report.outcomes {
        println!(
            "  - [{}] {}{}",
            apply_status_label(outcome.status),
            outcome.title,
            outcome
                .detail
                .as_deref()
                .map(|detail| format!(" ({detail})"))
                .unwrap_or_default()
        );
    }
}

fn apply_status_label(status: SettingsApplyStatus) -> &'static str {
    match status {
        SettingsApplyStatus::Applied => "applied",
        SettingsApplyStatus::Skipped => "skipped",
        SettingsApplyStatus::Unsupported => "unsupported",
        SettingsApplyStatus::Failed => "failed",
    }
}

fn count_failed_apply_steps(report: &SettingsApplyReport) -> usize {
    report
        .outcomes
        .iter()
        .filter(|outcome| outcome.status == SettingsApplyStatus::Failed)
        .count()
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn apply_config_set_args(settings: &mut Master3sSettings, args: &ConfigSetArgs) {
    if let Some(profile_name) = &args.profile_name {
        settings.profile_name = profile_name.clone();
    }
    if let Some(pointer_speed) = args.pointer_speed {
        settings.pointer_speed_percent = pointer_speed;
    }
    if args.smart_shift {
        settings.smart_shift_enabled = true;
        settings.ratchet_mode = WheelRatchetMode::SmartShift;
    }
    if args.no_smart_shift {
        settings.smart_shift_enabled = false;
        if settings.ratchet_mode == WheelRatchetMode::SmartShift {
            settings.ratchet_mode = WheelRatchetMode::Ratchet;
        }
    }
    if let Some(threshold) = args.smart_shift_threshold {
        settings.smart_shift_threshold = threshold;
    }
    if let Some(wheel_mode) = args.wheel_mode {
        settings.ratchet_mode = wheel_mode.into();
        settings.smart_shift_enabled = settings.ratchet_mode == WheelRatchetMode::SmartShift;
    }
    if args.high_resolution_scroll {
        settings.high_resolution_scroll = true;
    }
    if args.no_high_resolution_scroll {
        settings.high_resolution_scroll = false;
    }
    if args.natural_scroll {
        settings.natural_scroll = true;
    }
    if args.no_natural_scroll {
        settings.natural_scroll = false;
    }
    if let Some(thumb_wheel) = args.thumb_wheel {
        settings.thumb_wheel = thumb_wheel.into();
    }
    if let Some(thumb_wheel_speed) = args.thumb_wheel_speed {
        settings.thumb_wheel_speed_percent = thumb_wheel_speed;
    }
    for mapping in &args.button_mappings {
        settings.set_button_action(mapping.button, mapping.action);
    }
}

fn upsert_app_profile(
    settings: &mut Master3sSettings,
    args: &ConfigAppProfileSetArgs,
) -> Result<()> {
    let app_name = clean_app_profile_name(&args.app)?;
    let key = normalize_cli_name(&app_name);
    let profile = settings
        .app_profiles
        .iter_mut()
        .find(|profile| normalize_cli_name(&profile.app_name) == key);

    match profile {
        Some(profile) => {
            profile.app_name = app_name;
            if let Some(pointer_speed) = args.pointer_speed {
                profile.pointer_speed_percent = pointer_speed;
            }
            if let Some(thumb_wheel) = args.thumb_wheel {
                profile.thumb_wheel = thumb_wheel.into();
            }
        }
        None => settings.app_profiles.push(AppProfile {
            app_name,
            pointer_speed_percent: args.pointer_speed.unwrap_or(100),
            thumb_wheel: args
                .thumb_wheel
                .map(ThumbWheelMode::from)
                .unwrap_or(ThumbWheelMode::HorizontalScroll),
        }),
    }

    Ok(())
}

fn remove_app_profile(settings: &mut Master3sSettings, app_name: &str) -> Result<()> {
    let app_name = clean_app_profile_name(app_name)?;
    let key = normalize_cli_name(&app_name);
    let original_len = settings.app_profiles.len();
    settings
        .app_profiles
        .retain(|profile| normalize_cli_name(&profile.app_name) != key);

    if settings.app_profiles.len() == original_len {
        return Err(DogiError::InvalidArgument(format!(
            "app profile not found: {app_name}"
        )));
    }

    Ok(())
}

fn clean_app_profile_name(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(DogiError::InvalidArgument(
            "app profile name cannot be empty".to_owned(),
        ));
    }
    Ok(trimmed.to_owned())
}

fn enabled_text(value: bool) -> &'static str {
    if value { "enabled" } else { "disabled" }
}

fn print_json<T>(value: &T) -> Result<()>
where
    T: serde::Serialize,
{
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer_pretty(&mut handle, value)
        .map_err(|error| DogiError::Config(error.to_string()))?;
    writeln!(handle).map_err(|error| DogiError::Config(error.to_string()))
}

fn truncate(value: &str, width: usize) -> String {
    let mut chars = value.chars();
    let mut output = chars.by_ref().take(width).collect::<String>();

    if chars.next().is_some() && width > 1 {
        output.truncate(width - 1);
        output.push('~');
    }

    output
}

fn format_report_ids(device: &DeviceInfo) -> String {
    if device.report_descriptor.report_ids.is_empty() {
        "none".to_owned()
    } else {
        device
            .report_descriptor
            .report_ids
            .iter()
            .map(|id| format!("0x{id:02x}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn format_usage_pages(pages: &[u16]) -> String {
    if pages.is_empty() {
        "none".to_owned()
    } else {
        pages
            .iter()
            .map(|page| format!("0x{page:04x}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl From<CliWheelMode> for WheelRatchetMode {
    fn from(value: CliWheelMode) -> Self {
        match value {
            CliWheelMode::Ratchet => Self::Ratchet,
            CliWheelMode::FreeSpin => Self::FreeSpin,
            CliWheelMode::SmartShift => Self::SmartShift,
        }
    }
}

impl From<CliThumbWheelMode> for ThumbWheelMode {
    fn from(value: CliThumbWheelMode) -> Self {
        match value {
            CliThumbWheelMode::HorizontalScroll => Self::HorizontalScroll,
            CliThumbWheelMode::TabSwitch => Self::TabSwitch,
            CliThumbWheelMode::Zoom => Self::Zoom,
            CliThumbWheelMode::Volume => Self::Volume,
            CliThumbWheelMode::Disabled => Self::Disabled,
        }
    }
}

fn parse_button_mapping(value: &str) -> std::result::Result<CliButtonMapping, String> {
    let Some((button, action)) = value.split_once('=') else {
        return Err("expected BUTTON=ACTION, for example back=copy".to_owned());
    };

    Ok(CliButtonMapping {
        button: parse_master3s_button(button)?,
        action: parse_button_action(action)?,
    })
}

fn parse_master3s_button(value: &str) -> std::result::Result<Master3sButton, String> {
    match normalize_cli_name(value).as_str() {
        "back" | "backbutton" => Ok(Master3sButton::Back),
        "forward" | "forwardbutton" => Ok(Master3sButton::Forward),
        "gesture" | "gesturebutton" => Ok(Master3sButton::Gesture),
        "modeshift" | "modeshiftbutton" | "mode" => Ok(Master3sButton::ModeShift),
        "middle" | "middleclick" | "middlebutton" => Ok(Master3sButton::Middle),
        _ => Err(format!("unknown Master 3S button '{value}'")),
    }
}

fn parse_button_action(value: &str) -> std::result::Result<ButtonAction, String> {
    match normalize_cli_name(value).as_str() {
        "native" | "default" | "defaultbehavior" => Ok(ButtonAction::Native),
        "back" => Ok(ButtonAction::Back),
        "forward" => Ok(ButtonAction::Forward),
        "overview" | "activitiesoverview" | "missioncontrol" => Ok(ButtonAction::Overview),
        "switchwindows" | "windowswitcher" | "appexpose" | "expose" => {
            Ok(ButtonAction::WindowSwitcher)
        }
        "previousworkspace" | "desktopleft" | "leftdesktop" => Ok(ButtonAction::PreviousWorkspace),
        "nextworkspace" | "desktopright" | "rightdesktop" => Ok(ButtonAction::NextWorkspace),
        "middleclick" | "middle" => Ok(ButtonAction::MiddleClick),
        "copy" => Ok(ButtonAction::Copy),
        "paste" => Ok(ButtonAction::Paste),
        "disabled" | "disable" | "none" => Ok(ButtonAction::Disabled),
        _ => Err(format!("unknown button action '{value}'")),
    }
}

fn normalize_cli_name(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[derive(serde::Serialize)]
struct ConfigReport {
    path: String,
    settings: Master3sSettings,
    runtime_plan: LocalRuntimePlan,
}

impl ConfigReport {
    fn new(path: std::path::PathBuf, settings: Master3sSettings) -> Self {
        let settings = settings.normalized();
        let runtime_plan = build_master3s_runtime_plan(&settings);

        Self {
            path: path.display().to_string(),
            settings,
            runtime_plan,
        }
    }
}

#[derive(serde::Serialize)]
struct RuntimeListenReport {
    device_id: String,
    runtime_plan: LocalRuntimePlan,
    events: Vec<Master3sRuntimeEvent>,
    actions: Vec<ResolvedRuntimeAction>,
    executions: Vec<RuntimeActionExecution>,
    execute_actions: bool,
}

#[derive(serde::Serialize)]
struct DoctorReport {
    hid_backend: &'static str,
    device_writer_status: &'static str,
    total_hid_devices: usize,
    logitech_devices: usize,
    hidpp_interfaces: usize,
    hidraw_readable: usize,
    hidraw_readwrite: usize,
    linux_permission_hint: Option<&'static str>,
    linux_udev_rule_path: Option<&'static str>,
    linux_udev_rule: Option<&'static str>,
    linux_udev_reload_hint: Option<&'static str>,
    runtime_uinput_path: Option<&'static str>,
    runtime_uinput_writable: bool,
    runtime_uinput_detail: String,
    runtime_uinput_hint: Option<&'static str>,
    active_window_backend: Option<&'static str>,
    active_window_available: bool,
    active_window_detail: String,
    systemd_user_service_path: Option<String>,
    systemd_user_service_installed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_values() {
        assert_eq!(truncate("dogi", 12), "dogi");
    }

    #[test]
    fn truncate_marks_long_values() {
        assert_eq!(truncate("0123456789", 5), "0123~");
    }

    #[test]
    fn no_arguments_default_to_gui() {
        let cli = Cli::try_parse_from(["dogi"]).expect("CLI should parse without arguments");

        assert!(matches!(command_or_default(cli), Command::Gui));
    }

    #[test]
    fn arguments_select_cli_commands() {
        let cli = Cli::try_parse_from(["dogi", "doctor"]).expect("doctor command should parse");

        assert!(matches!(command_or_default(cli), Command::Doctor(_)));
    }

    #[test]
    fn config_command_parses() {
        let cli = Cli::try_parse_from([
            "dogi",
            "config",
            "set",
            "--pointer-speed",
            "120",
            "--wheel-mode",
            "free-spin",
            "--thumb-wheel-speed",
            "140",
            "--button",
            "gesture=copy",
        ])
        .expect("config set command should parse");

        assert!(matches!(command_or_default(cli), Command::Config(_)));
    }

    #[test]
    fn config_app_profile_commands_parse() {
        let list = Cli::try_parse_from(["dogi", "config", "app-profile", "list"])
            .expect("app-profile list parses");
        let set = Cli::try_parse_from([
            "dogi",
            "config",
            "app-profile",
            "set",
            "--app",
            "firefox",
            "--pointer-speed",
            "90",
            "--thumb-wheel",
            "tab-switch",
        ])
        .expect("app-profile set parses");
        let remove = Cli::try_parse_from([
            "dogi",
            "config",
            "app-profile",
            "remove",
            "--app",
            "firefox",
        ])
        .expect("app-profile remove parses");

        assert!(matches!(command_or_default(list), Command::Config(_)));
        assert!(matches!(command_or_default(set), Command::Config(_)));
        assert!(matches!(command_or_default(remove), Command::Config(_)));
    }

    #[test]
    fn runtime_commands_parse() {
        let plan = Cli::try_parse_from(["dogi", "runtime", "plan"]).expect("runtime plan parses");
        let listen = Cli::try_parse_from([
            "dogi",
            "runtime",
            "listen",
            "--device-id",
            "device-1",
            "--events",
            "2",
            "--idle-timeout-ms",
            "100",
            "--execute-actions",
        ])
        .expect("runtime listen parses");
        let run = Cli::try_parse_from([
            "dogi",
            "runtime",
            "run",
            "--device-id",
            "device-1",
            "--max-events",
            "3",
            "--execute-actions",
            "--allow-device-write",
        ])
        .expect("runtime run parses");
        let auto_listen = Cli::try_parse_from(["dogi", "runtime", "listen"])
            .expect("runtime listen can auto-select device");
        let auto_run = Cli::try_parse_from(["dogi", "runtime", "run"])
            .expect("runtime run can auto-select device");

        assert!(matches!(command_or_default(plan), Command::Runtime(_)));
        assert!(matches!(command_or_default(listen), Command::Runtime(_)));
        assert!(matches!(command_or_default(run), Command::Runtime(_)));
        assert!(matches!(
            command_or_default(auto_listen),
            Command::Runtime(_)
        ));
        assert!(matches!(command_or_default(auto_run), Command::Runtime(_)));
    }

    #[test]
    fn runtime_effective_state_adopts_initial_and_changed_settings() {
        let base = Master3sSettings::default();
        let mut state = RuntimeEffectiveState::default();

        assert!(state.needs_update(None, &base, false));

        state.update(None, &base, false);

        assert!(!state.needs_update(None, &base, false));
        assert!(state.needs_update(None, &base, true));
        assert!(state.needs_update(Some("Firefox"), &base, false));

        let faster = Master3sSettings {
            pointer_speed_percent: 135,
            ..Master3sSettings::default()
        };

        assert!(state.needs_update(None, &faster, false));
        assert!(state.settings_changed(&faster));

        state.update(Some("Firefox".to_owned()), &faster, false);

        assert!(!state.needs_update(Some("Firefox"), &faster, false));

        let edited_profile = Master3sSettings {
            pointer_speed_percent: 145,
            ..Master3sSettings::default()
        };
        assert!(state.needs_update(Some("Firefox"), &edited_profile, false));

        assert!(state.needs_update(Some("Code"), &faster, false));
    }

    #[test]
    fn runtime_preview_only_forces_thumb_wheel_diversion() {
        let base = Master3sSettings {
            pointer_speed_percent: 135,
            thumb_wheel: ThumbWheelMode::Zoom,
            thumb_wheel_speed_percent: 250,
            ..Master3sSettings::default()
        };
        let preview = runtime_control::HorizontalScrollPreview {
            lease_id: "lease".to_owned(),
            device_id: "device".to_owned(),
            speed_percent: 175,
        };

        let target = runtime_device_settings(&base, Some(&preview));

        assert_eq!(target.pointer_speed_percent, 135);
        assert_eq!(target.thumb_wheel, ThumbWheelMode::HorizontalScroll);
        assert_eq!(
            target.thumb_wheel_speed_percent,
            PREVIEW_DIVERSION_SPEED_PERCENT
        );
        assert_eq!(runtime_device_settings(&base, None), base.normalized());
    }

    #[test]
    fn runtime_preview_transition_always_writes_thumb_wheel_routing() {
        let settings = Master3sSettings {
            thumb_wheel: ThumbWheelMode::HorizontalScroll,
            thumb_wheel_speed_percent: PREVIEW_DIVERSION_SPEED_PERCENT,
            ..Master3sSettings::default()
        };

        let ordinary = runtime_device_apply_plan("device", Some(&settings), &settings, false);
        let preview = runtime_device_apply_plan("device", Some(&settings), &settings, true);

        assert!(ordinary.steps.is_empty());
        assert_eq!(preview.steps.len(), 1);
        assert_eq!(preview.steps[0].feature, HidppFeature::ThumbWheel);
    }

    #[test]
    fn runtime_startup_never_reconciles_saved_defaults_to_the_mouse() {
        let settings = Master3sSettings::default();

        let plan = runtime_device_apply_plan("device", None, &settings, false);

        assert!(plan.steps.is_empty());
    }

    #[test]
    fn runtime_reconnect_during_preview_writes_only_thumb_wheel_routing() {
        let settings = Master3sSettings {
            pointer_speed_percent: 175,
            high_resolution_scroll: true,
            natural_scroll: true,
            thumb_wheel: ThumbWheelMode::HorizontalScroll,
            thumb_wheel_speed_percent: PREVIEW_DIVERSION_SPEED_PERCENT,
            ..Master3sSettings::default()
        };

        let plan = runtime_device_apply_plan("device", None, &settings, true);

        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].feature, HidppFeature::ThumbWheel);
    }

    #[test]
    fn runtime_reload_updates_base_settings_from_saved_config() {
        let path = unique_cli_test_path("runtime-reload");
        let daemon = DeviceService::with_config_path(&path);
        let mut current = Master3sSettings::default();
        let mut warning_printed = true;
        let saved = Master3sSettings {
            pointer_speed_percent: 135,
            ..Master3sSettings::default()
        };

        daemon
            .save_master3s_settings_for_device("device-a", &saved)
            .unwrap();

        assert!(reload_runtime_base_settings(
            &daemon,
            "device-a",
            &mut current,
            &mut warning_printed
        ));
        assert_eq!(current.pointer_speed_percent, 135);
        assert!(!warning_printed);
        assert!(!reload_runtime_base_settings(
            &daemon,
            "device-a",
            &mut current,
            &mut warning_printed
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn service_commands_parse() {
        let print =
            Cli::try_parse_from(["dogi", "service", "print"]).expect("service print parses");
        let install =
            Cli::try_parse_from(["dogi", "service", "install"]).expect("service install parses");
        let uninstall = Cli::try_parse_from(["dogi", "service", "uninstall"])
            .expect("service uninstall parses");

        assert!(matches!(command_or_default(print), Command::Service(_)));
        assert!(matches!(command_or_default(install), Command::Service(_)));
        assert!(matches!(command_or_default(uninstall), Command::Service(_)));
    }

    #[test]
    fn udev_commands_parse() {
        let print = Cli::try_parse_from(["dogi", "udev", "print"]).expect("udev print parses");
        let install =
            Cli::try_parse_from(["dogi", "udev", "install"]).expect("udev install parses");
        let uninstall =
            Cli::try_parse_from(["dogi", "udev", "uninstall"]).expect("udev uninstall parses");
        let path = Cli::try_parse_from(["dogi", "udev", "path"]).expect("udev path parses");

        assert!(matches!(command_or_default(print), Command::Udev(_)));
        assert!(matches!(command_or_default(install), Command::Udev(_)));
        assert!(matches!(command_or_default(uninstall), Command::Udev(_)));
        assert!(matches!(command_or_default(path), Command::Udev(_)));
    }

    #[test]
    fn udev_rules_cover_hidraw_and_uinput() {
        assert!(LINUX_UDEV_RULE.contains("hidraw"));
        assert!(LINUX_UDEV_RULE.contains("ATTRS{idVendor}==\"046d\""));
        assert!(LINUX_UDEV_RULE.contains("uinput"));
        assert!(LINUX_UDEV_RULE.contains("static_node=uinput"));
        assert!(LINUX_UDEV_RULE.matches("TAG+=\"uaccess\"").count() >= 2);
    }

    #[test]
    fn auto_selects_single_runtime_device() {
        let devices = vec![
            make_runtime_device("hidraw0", false, false),
            make_runtime_device("hidraw1", true, true),
        ];

        assert_eq!(select_runtime_device_id(&devices).unwrap(), "hidraw1");
    }

    #[test]
    fn auto_select_requires_explicit_id_for_multiple_runtime_devices() {
        let devices = vec![
            make_runtime_device("hidraw1", true, true),
            make_runtime_device("hidraw2", true, true),
        ];

        let error = select_runtime_device_id(&devices).unwrap_err().to_string();

        assert!(error.contains("multiple paired HID++ Logitech devices"));
        assert!(error.contains("hidraw1"));
        assert!(error.contains("hidraw2"));
    }

    #[test]
    fn config_apply_command_requires_explicit_flag_in_args_shape() {
        let cli = Cli::try_parse_from([
            "dogi",
            "config",
            "apply",
            "--device-id",
            "device-1",
            "--allow-device-write",
        ])
        .expect("config apply command should parse");

        assert!(matches!(command_or_default(cli), Command::Config(_)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn doctor_report_includes_logitech_runtime_diagnostics() {
        let report = doctor_report_from_devices_with_environment(
            &[],
            RuntimeEnvironmentStatus {
                available: true,
                detail: "uinput ok".to_owned(),
            },
            RuntimeEnvironmentStatus {
                available: false,
                detail: "xprop missing".to_owned(),
            },
            SystemdUserServiceStatus {
                path: Some("/home/test/.config/systemd/user/dogi-runtime.service".to_owned()),
                installed: false,
            },
        );

        assert_eq!(
            report.linux_udev_rule_path,
            Some("/etc/udev/rules.d/70-dogi-logitech.rules")
        );
        assert!(
            report
                .linux_udev_rule
                .is_some_and(|rule| rule.contains("hidraw")
                    && rule.contains("046d")
                    && rule.contains("uinput"))
        );
        assert!(
            report
                .linux_udev_reload_hint
                .is_some_and(|hint| hint.contains("udevadm"))
        );
        assert_eq!(report.runtime_uinput_path, Some("/dev/uinput"));
        assert!(report.runtime_uinput_writable);
        assert_eq!(report.runtime_uinput_detail, "uinput ok");
        assert_eq!(report.active_window_backend, Some("xprop"));
        assert!(!report.active_window_available);
        assert_eq!(report.active_window_detail, "xprop missing");
        assert_eq!(
            report.systemd_user_service_path.as_deref(),
            Some("/home/test/.config/systemd/user/dogi-runtime.service")
        );
        assert!(!report.systemd_user_service_installed);
    }

    #[test]
    fn config_report_includes_runtime_plan() {
        let settings = Master3sSettings {
            thumb_wheel: ThumbWheelMode::Zoom,
            ..Master3sSettings::default()
        };
        let report = ConfigReport::new(std::path::PathBuf::from("/tmp/dogi.json"), settings);

        assert!(report.runtime_plan.requires_listener());
        assert!(report.runtime_plan.summary().contains("thumb wheel zoom"));
    }

    #[test]
    fn formats_runtime_events() {
        assert_eq!(
            format_runtime_event(&Master3sRuntimeEvent::ThumbWheel {
                delta: -12,
                phase: Some(1),
                resolution: 8,
                direction: -1,
            }),
            "thumb wheel delta -12, phase 1, resolution 8, direction -1"
        );
        assert_eq!(
            format_runtime_event(&Master3sRuntimeEvent::DivertedButtons {
                buttons: vec![Master3sButton::Back],
                unknown_control_ids: vec![0xbeef],
            }),
            "diverted buttons Back button; unknown 0xBEEF"
        );
    }

    #[test]
    fn formats_runtime_actions() {
        assert_eq!(
            format_resolved_runtime_action(&ResolvedRuntimeAction {
                source: RuntimeActionSource::Button(Master3sButton::Back),
                command: dogi_core::RuntimeCommand::KeyChord(vec![
                    dogi_core::RuntimeKey::Alt,
                    dogi_core::RuntimeKey::Left,
                ]),
            }),
            "Back button -> Alt+Left"
        );
        assert_eq!(
            format_resolved_runtime_action(&ResolvedRuntimeAction {
                source: RuntimeActionSource::UnknownControlId(0xbeef),
                command: dogi_core::RuntimeCommand::Unsupported,
            }),
            "unknown control 0xBEEF -> unsupported"
        );
    }

    #[test]
    fn formats_runtime_action_executions() {
        assert_eq!(
            format_runtime_action_execution(&RuntimeActionExecution {
                action: ResolvedRuntimeAction {
                    source: RuntimeActionSource::ThumbWheel,
                    command: dogi_core::RuntimeCommand::KeyChord(vec![
                        dogi_core::RuntimeKey::Control,
                        dogi_core::RuntimeKey::Equal,
                    ]),
                },
                status: RuntimeActionExecutionStatus::Executed,
                detail: None,
            }),
            "thumb wheel -> Ctrl+= [executed]"
        );
        assert_eq!(
            format_runtime_action_execution(&RuntimeActionExecution {
                action: ResolvedRuntimeAction {
                    source: RuntimeActionSource::UnknownControlId(0xbeef),
                    command: dogi_core::RuntimeCommand::Unsupported,
                },
                status: RuntimeActionExecutionStatus::Unsupported,
                detail: Some("cannot execute this runtime command".to_owned()),
            }),
            "unknown control 0xBEEF -> unsupported [unsupported]: cannot execute this runtime command"
        );
    }

    #[test]
    fn config_set_updates_button_mappings() {
        let args = ConfigSetArgs {
            profile_name: None,
            pointer_speed: None,
            smart_shift: false,
            no_smart_shift: false,
            smart_shift_threshold: None,
            wheel_mode: None,
            high_resolution_scroll: false,
            no_high_resolution_scroll: false,
            natural_scroll: false,
            no_natural_scroll: false,
            thumb_wheel: None,
            thumb_wheel_speed: None,
            button_mappings: vec![parse_button_mapping("gesture=copy").unwrap()],
            json: false,
        };
        let mut settings = Master3sSettings::default();

        apply_config_set_args(&mut settings, &args);

        assert_eq!(
            settings.button_action(Master3sButton::Gesture),
            ButtonAction::Copy
        );
    }

    #[test]
    fn app_profile_set_adds_new_profile_with_defaults() {
        let mut settings = Master3sSettings {
            app_profiles: Vec::new(),
            ..Master3sSettings::default()
        };
        let args = ConfigAppProfileSetArgs {
            app: " firefox ".to_owned(),
            pointer_speed: None,
            thumb_wheel: None,
            json: false,
        };

        upsert_app_profile(&mut settings, &args).unwrap();

        assert_eq!(settings.app_profiles.len(), 1);
        assert_eq!(settings.app_profiles[0].app_name, "firefox");
        assert_eq!(settings.app_profiles[0].pointer_speed_percent, 100);
        assert_eq!(
            settings.app_profiles[0].thumb_wheel,
            ThumbWheelMode::HorizontalScroll
        );
    }

    #[test]
    fn app_profile_set_updates_existing_profile_without_resetting_unspecified_fields() {
        let mut settings = Master3sSettings {
            app_profiles: vec![AppProfile {
                app_name: "Firefox".to_owned(),
                pointer_speed_percent: 80,
                thumb_wheel: ThumbWheelMode::Zoom,
            }],
            ..Master3sSettings::default()
        };
        let args = ConfigAppProfileSetArgs {
            app: "firefox".to_owned(),
            pointer_speed: Some(95),
            thumb_wheel: None,
            json: false,
        };

        upsert_app_profile(&mut settings, &args).unwrap();

        assert_eq!(settings.app_profiles.len(), 1);
        assert_eq!(settings.app_profiles[0].app_name, "firefox");
        assert_eq!(settings.app_profiles[0].pointer_speed_percent, 95);
        assert_eq!(settings.app_profiles[0].thumb_wheel, ThumbWheelMode::Zoom);
    }

    #[test]
    fn app_profile_remove_deletes_by_normalized_name() {
        let mut settings = Master3sSettings {
            app_profiles: vec![AppProfile {
                app_name: "Visual Studio Code".to_owned(),
                pointer_speed_percent: 90,
                thumb_wheel: ThumbWheelMode::HorizontalScroll,
            }],
            ..Master3sSettings::default()
        };

        remove_app_profile(&mut settings, "visual-studio-code").unwrap();

        assert!(settings.app_profiles.is_empty());
    }

    #[test]
    fn app_profile_remove_reports_missing_profile() {
        let mut settings = Master3sSettings {
            app_profiles: Vec::new(),
            ..Master3sSettings::default()
        };

        let error = remove_app_profile(&mut settings, "firefox")
            .unwrap_err()
            .to_string();

        assert!(error.contains("app profile not found"));
    }

    #[test]
    fn rejects_invalid_button_mapping() {
        assert!(parse_button_mapping("gesture").is_err());
        assert!(parse_button_mapping("gesture=launchpad").is_err());
    }

    #[test]
    fn display_device_name_uses_known_model_id_when_hidpp_name_is_missing() {
        let mut device = make_runtime_device("hidraw0", true, true);
        let paired = device.paired_device.as_mut().unwrap();
        paired.name = None;
        paired.model_id = Some("B03400000000".to_owned());

        assert_eq!(display_device_name(&device), "MX Master 3S");
    }

    #[test]
    fn display_device_name_uses_receiver_wpid_when_feature_name_is_missing() {
        let mut device = make_runtime_device("hidraw0", true, true);
        let paired = device.paired_device.as_mut().unwrap();
        paired.name = None;
        paired.model_id = None;
        paired.wpid = Some("B034".to_owned());

        assert_eq!(display_device_name(&device), "MX Master 3S");
    }

    #[test]
    fn display_device_name_does_not_map_receiver_product_id_to_mouse() {
        let device = make_runtime_device("hidraw0", true, false);

        assert_eq!(display_device_name(&device), "Logitech USB Receiver");
    }

    fn make_runtime_device(id: &str, hidpp: bool, paired: bool) -> DeviceInfo {
        DeviceInfo {
            id: id.to_owned(),
            name: "Logitech USB Receiver".to_owned(),
            paired_device: paired.then(|| dogi_core::PairedDeviceInfo {
                slot: 1,
                name: Some("MX Master 3S".to_owned()),
                kind: Some("mouse".to_owned()),
                wpid: None,
                protocol: None,
                unit_id: None,
                model_id: None,
                feature_count: 0,
                features: Vec::new(),
            }),
            manufacturer: Some("Logitech".to_owned()),
            serial_number: None,
            bus: dogi_core::BusKind::Usb,
            bus_id: Some(0x0003),
            vendor_id: LOGITECH_VENDOR_ID,
            product_id: 0xc548,
            release_number: None,
            connection: dogi_core::ConnectionKind::Bolt,
            receiver_kind: Some(dogi_core::ReceiverKind::Bolt),
            path: format!("/dev/{id}"),
            sysfs_path: format!("/sys/class/hidraw/{id}"),
            physical_path: None,
            driver: Some("hid-generic".to_owned()),
            interface_number: None,
            usage_page: None,
            usage: None,
            access: dogi_core::DeviceAccess::default(),
            battery: dogi_core::BatteryInfo::not_queried("test"),
            report_descriptor: dogi_core::ReportDescriptorInfo {
                hidpp_usage: hidpp.then_some(dogi_core::HidUsage {
                    usage_page: 0xff00,
                    usage: 0x0001,
                }),
                ..dogi_core::ReportDescriptorInfo::default()
            },
            capabilities: dogi_core::DeviceCapabilities {
                hidpp: if hidpp {
                    CapabilityState::Supported
                } else {
                    CapabilityState::Unknown
                },
                ..dogi_core::DeviceCapabilities::default()
            },
        }
    }

    fn unique_cli_test_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("dogi-command-{name}-{}.json", std::process::id()))
    }
}
