use dogi_core::{
    DogiError, ResolvedRuntimeAction, Result, RuntimeCommand, RuntimeKey, RuntimeMouseButton,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeActionExecution {
    pub action: ResolvedRuntimeAction,
    pub status: RuntimeActionExecutionStatus,
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeActionExecutionStatus {
    Executed,
    Skipped,
    Unsupported,
    Failed,
}

impl RuntimeActionExecutionStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Executed => "executed",
            Self::Skipped => "skipped",
            Self::Unsupported => "unsupported",
            Self::Failed => "failed",
        }
    }
}

pub trait RuntimeActionExecutor {
    fn execute_command(&mut self, command: &RuntimeCommand) -> Result<()>;
}

pub fn execute_runtime_actions_with(
    actions: &[ResolvedRuntimeAction],
    executor: &mut impl RuntimeActionExecutor,
) -> Vec<RuntimeActionExecution> {
    actions
        .iter()
        .map(|action| execute_runtime_action_with(action, executor))
        .collect()
}

pub fn execute_runtime_actions(
    actions: &[ResolvedRuntimeAction],
) -> Result<Vec<RuntimeActionExecution>> {
    let mut executor = None;
    let mut executions = Vec::with_capacity(actions.len());

    for action in actions {
        let execution = match &action.command {
            RuntimeCommand::Noop | RuntimeCommand::Unsupported => RuntimeActionExecution {
                action: action.clone(),
                status: runtime_action_skip_status(&action.command),
                detail: runtime_action_skip_detail(&action.command),
            },
            _ => {
                if executor.is_none() {
                    executor = Some(SystemRuntimeActionExecutor::open()?);
                }
                execute_runtime_action_with(
                    action,
                    executor
                        .as_mut()
                        .expect("executor exists after successful open"),
                )
            }
        };
        executions.push(execution);
    }

    Ok(executions)
}

fn execute_runtime_action_with(
    action: &ResolvedRuntimeAction,
    executor: &mut impl RuntimeActionExecutor,
) -> RuntimeActionExecution {
    match &action.command {
        RuntimeCommand::Noop => RuntimeActionExecution {
            action: action.clone(),
            status: runtime_action_skip_status(&action.command),
            detail: runtime_action_skip_detail(&action.command),
        },
        RuntimeCommand::Unsupported => RuntimeActionExecution {
            action: action.clone(),
            status: runtime_action_skip_status(&action.command),
            detail: runtime_action_skip_detail(&action.command),
        },
        command => match executor.execute_command(command) {
            Ok(()) => RuntimeActionExecution {
                action: action.clone(),
                status: RuntimeActionExecutionStatus::Executed,
                detail: None,
            },
            Err(error) => RuntimeActionExecution {
                action: action.clone(),
                status: RuntimeActionExecutionStatus::Failed,
                detail: Some(error.to_string()),
            },
        },
    }
}

fn runtime_action_skip_status(command: &RuntimeCommand) -> RuntimeActionExecutionStatus {
    match command {
        RuntimeCommand::Noop => RuntimeActionExecutionStatus::Skipped,
        RuntimeCommand::Unsupported => RuntimeActionExecutionStatus::Unsupported,
        _ => RuntimeActionExecutionStatus::Executed,
    }
}

fn runtime_action_skip_detail(command: &RuntimeCommand) -> Option<String> {
    match command {
        RuntimeCommand::Noop => Some("no action configured".to_owned()),
        RuntimeCommand::Unsupported => Some("cannot execute this runtime command".to_owned()),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write};
    use std::mem;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::slice;
    use std::thread;
    use std::time::Duration;

    const UINPUT_PATH: &str = "/dev/uinput";
    const UINPUT_MAX_NAME_SIZE: usize = 80;
    const ABS_CNT: usize = 64;

    const EV_SYN: u16 = 0x00;
    const EV_KEY: u16 = 0x01;
    const EV_REL: u16 = 0x02;
    const SYN_REPORT: u16 = 0;
    const REL_HWHEEL: u16 = 0x06;
    const REL_HWHEEL_HI_RES: u16 = 0x0c;

    const BUS_USB: u16 = 0x03;
    const DOGI_UINPUT_VENDOR: u16 = 0x046d;
    const DOGI_UINPUT_PRODUCT: u16 = 0xd061;

    const KEY_MINUS: u16 = 12;
    const KEY_EQUAL: u16 = 13;
    const KEY_TAB: u16 = 15;
    const KEY_LEFTCTRL: u16 = 29;
    const KEY_LEFTSHIFT: u16 = 42;
    const KEY_C: u16 = 46;
    const KEY_V: u16 = 47;
    const KEY_LEFTALT: u16 = 56;
    const KEY_RIGHT: u16 = 106;
    const KEY_LEFT: u16 = 105;
    const KEY_VOLUMEDOWN: u16 = 114;
    const KEY_VOLUMEUP: u16 = 115;
    const KEY_LEFTMETA: u16 = 125;
    const BTN_MIDDLE: u16 = 274;

    const IOC_NRBITS: u32 = 8;
    const IOC_TYPEBITS: u32 = 8;
    const IOC_SIZEBITS: u32 = 14;

    const IOC_NRSHIFT: u32 = 0;
    const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
    const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
    const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;
    const IOC_WRITE: u32 = 1;

    const fn ioctl_ioc(dir: u32, ty: u32, nr: u32, size: u32) -> libc::c_ulong {
        ((dir << IOC_DIRSHIFT)
            | (ty << IOC_TYPESHIFT)
            | (nr << IOC_NRSHIFT)
            | (size << IOC_SIZESHIFT)) as libc::c_ulong
    }

    const fn ioctl_io(ty: u8, nr: u8) -> libc::c_ulong {
        ioctl_ioc(0, ty as u32, nr as u32, 0)
    }

    const fn ioctl_iow(ty: u8, nr: u8, size: u32) -> libc::c_ulong {
        ioctl_ioc(IOC_WRITE, ty as u32, nr as u32, size)
    }

    const UI_DEV_CREATE: libc::c_ulong = ioctl_io(b'U', 1);
    const UI_DEV_DESTROY: libc::c_ulong = ioctl_io(b'U', 2);
    const UI_SET_EVBIT: libc::c_ulong = ioctl_iow(b'U', 100, mem::size_of::<libc::c_int>() as u32);
    const UI_SET_KEYBIT: libc::c_ulong = ioctl_iow(b'U', 101, mem::size_of::<libc::c_int>() as u32);
    const UI_SET_RELBIT: libc::c_ulong = ioctl_iow(b'U', 102, mem::size_of::<libc::c_int>() as u32);

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct InputId {
        bustype: u16,
        vendor: u16,
        product: u16,
        version: u16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct InputEvent {
        time: libc::timeval,
        type_: u16,
        code: u16,
        value: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct UInputUserDev {
        name: [libc::c_char; UINPUT_MAX_NAME_SIZE],
        id: InputId,
        ff_effects_max: u32,
        absmax: [i32; ABS_CNT],
        absmin: [i32; ABS_CNT],
        absfuzz: [i32; ABS_CNT],
        absflat: [i32; ABS_CNT],
    }

    impl Default for UInputUserDev {
        fn default() -> Self {
            Self {
                name: [0; UINPUT_MAX_NAME_SIZE],
                id: InputId {
                    bustype: 0,
                    vendor: 0,
                    product: 0,
                    version: 0,
                },
                ff_effects_max: 0,
                absmax: [0; ABS_CNT],
                absmin: [0; ABS_CNT],
                absfuzz: [0; ABS_CNT],
                absflat: [0; ABS_CNT],
            }
        }
    }

    pub struct SystemRuntimeActionExecutor {
        device: UInputDevice,
    }

    impl SystemRuntimeActionExecutor {
        pub fn open() -> Result<Self> {
            Ok(Self {
                device: UInputDevice::create()?,
            })
        }
    }

    impl RuntimeActionExecutor for SystemRuntimeActionExecutor {
        fn execute_command(&mut self, command: &RuntimeCommand) -> Result<()> {
            self.device.execute_command(command)
        }
    }

    struct UInputDevice {
        file: File,
        horizontal_scroll: HorizontalScrollAccumulator,
    }

    impl UInputDevice {
        fn create() -> Result<Self> {
            let mut file = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(UINPUT_PATH)
                .map_err(|error| {
                    DogiError::BackendUnavailable(format!(
                        "failed to open {UINPUT_PATH} for local runtime actions: {error}"
                    ))
                })?;

            ioctl_int(&file, UI_SET_EVBIT, EV_KEY as libc::c_int)?;
            for code in supported_key_codes() {
                ioctl_int(&file, UI_SET_KEYBIT, code as libc::c_int)?;
            }
            ioctl_int(&file, UI_SET_EVBIT, EV_REL as libc::c_int)?;
            ioctl_int(&file, UI_SET_RELBIT, REL_HWHEEL as libc::c_int)?;
            ioctl_int(&file, UI_SET_RELBIT, REL_HWHEEL_HI_RES as libc::c_int)?;

            let user_dev = dogi_user_dev();
            file.write_all(as_bytes(&user_dev)).map_err(|error| {
                DogiError::Transport(format!("failed to register uinput device: {error}"))
            })?;
            ioctl_none(&file, UI_DEV_CREATE)?;
            thread::sleep(Duration::from_millis(100));

            Ok(Self {
                file,
                horizontal_scroll: HorizontalScrollAccumulator::default(),
            })
        }

        fn execute_command(&mut self, command: &RuntimeCommand) -> Result<()> {
            match command {
                RuntimeCommand::KeyChord(keys) => self.key_chord(keys),
                RuntimeCommand::MouseButton(RuntimeMouseButton::Middle) => self.click(BTN_MIDDLE),
                RuntimeCommand::HorizontalScroll {
                    delta,
                    resolution,
                    direction,
                    speed_percent,
                } => self.horizontal_scroll(*delta, *resolution, *direction, *speed_percent),
                RuntimeCommand::Noop | RuntimeCommand::Unsupported => Ok(()),
            }
        }

        fn horizontal_scroll(
            &mut self,
            delta: i16,
            resolution: u16,
            direction: i8,
            speed_percent: u16,
        ) -> Result<()> {
            let movement =
                self.horizontal_scroll
                    .scale(delta, resolution, direction, speed_percent);
            if movement.high_resolution == 0 && movement.low_resolution == 0 {
                return Ok(());
            }
            if movement.low_resolution != 0 {
                self.emit(EV_REL, REL_HWHEEL, movement.low_resolution)?;
            }
            if movement.high_resolution != 0 {
                self.emit(EV_REL, REL_HWHEEL_HI_RES, movement.high_resolution)?;
            }
            self.sync()
        }

        fn key_chord(&mut self, keys: &[RuntimeKey]) -> Result<()> {
            for key in keys {
                self.emit(EV_KEY, linux_key_code(*key), 1)?;
            }
            self.sync()?;

            for key in keys.iter().rev() {
                self.emit(EV_KEY, linux_key_code(*key), 0)?;
            }
            self.sync()
        }

        fn click(&mut self, button_code: u16) -> Result<()> {
            self.emit(EV_KEY, button_code, 1)?;
            self.sync()?;
            self.emit(EV_KEY, button_code, 0)?;
            self.sync()
        }

        fn sync(&mut self) -> Result<()> {
            self.emit(EV_SYN, SYN_REPORT, 0)
        }

        fn emit(&mut self, type_: u16, code: u16, value: i32) -> Result<()> {
            let event = InputEvent {
                time: libc::timeval {
                    tv_sec: 0,
                    tv_usec: 0,
                },
                type_,
                code,
                value,
            };
            self.file.write_all(as_bytes(&event)).map_err(|error| {
                DogiError::Transport(format!("failed to write uinput event: {error}"))
            })
        }
    }

    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    struct HorizontalScrollMovement {
        high_resolution: i32,
        low_resolution: i32,
    }

    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    struct HorizontalScrollAccumulator {
        signature: Option<(u16, i8, u16)>,
        scale_remainder: i64,
        low_resolution_remainder: i64,
    }

    impl HorizontalScrollAccumulator {
        fn scale(
            &mut self,
            delta: i16,
            resolution: u16,
            direction: i8,
            speed_percent: u16,
        ) -> HorizontalScrollMovement {
            let resolution = resolution.max(1);
            let direction = if direction < 0 { -1_i8 } else { 1_i8 };
            let speed_percent = speed_percent.clamp(
                dogi_core::MIN_THUMB_WHEEL_SPEED_PERCENT,
                dogi_core::MAX_THUMB_WHEEL_SPEED_PERCENT,
            );
            let signature = (resolution, direction, speed_percent);
            if self.signature != Some(signature) {
                self.signature = Some(signature);
                self.scale_remainder = 0;
                self.low_resolution_remainder = 0;
            }

            let scaled_numerator = i64::from(delta)
                * i64::from(resolution)
                * i64::from(direction)
                * i64::from(speed_percent)
                + self.scale_remainder;
            let high_resolution = scaled_numerator / 100;
            self.scale_remainder = scaled_numerator % 100;

            self.low_resolution_remainder += high_resolution;
            let low_resolution = self.low_resolution_remainder / 120;
            self.low_resolution_remainder %= 120;

            HorizontalScrollMovement {
                high_resolution: saturating_i32(high_resolution),
                low_resolution: saturating_i32(low_resolution),
            }
        }
    }

    fn saturating_i32(value: i64) -> i32 {
        value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    }

    impl Drop for UInputDevice {
        fn drop(&mut self) {
            let _ = unsafe { libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY) };
        }
    }

    fn dogi_user_dev() -> UInputUserDev {
        let mut user_dev = UInputUserDev::default();
        let name = b"dogi virtual input\0";
        for (slot, byte) in user_dev.name.iter_mut().zip(name.iter().copied()) {
            *slot = byte as libc::c_char;
        }
        user_dev.id = InputId {
            bustype: BUS_USB,
            vendor: DOGI_UINPUT_VENDOR,
            product: DOGI_UINPUT_PRODUCT,
            version: 1,
        };
        user_dev
    }

    fn supported_key_codes() -> [u16; 14] {
        [
            KEY_MINUS,
            KEY_EQUAL,
            KEY_TAB,
            KEY_LEFTCTRL,
            KEY_LEFTSHIFT,
            KEY_C,
            KEY_V,
            KEY_LEFTALT,
            KEY_LEFT,
            KEY_RIGHT,
            KEY_VOLUMEDOWN,
            KEY_VOLUMEUP,
            KEY_LEFTMETA,
            BTN_MIDDLE,
        ]
    }

    fn linux_key_code(key: RuntimeKey) -> u16 {
        match key {
            RuntimeKey::Control => KEY_LEFTCTRL,
            RuntimeKey::Shift => KEY_LEFTSHIFT,
            RuntimeKey::Alt => KEY_LEFTALT,
            RuntimeKey::Super => KEY_LEFTMETA,
            RuntimeKey::Tab => KEY_TAB,
            RuntimeKey::Equal => KEY_EQUAL,
            RuntimeKey::Minus => KEY_MINUS,
            RuntimeKey::Left => KEY_LEFT,
            RuntimeKey::Right => KEY_RIGHT,
            RuntimeKey::C => KEY_C,
            RuntimeKey::V => KEY_V,
            RuntimeKey::VolumeUp => KEY_VOLUMEUP,
            RuntimeKey::VolumeDown => KEY_VOLUMEDOWN,
        }
    }

    fn ioctl_int(file: &File, request: libc::c_ulong, value: libc::c_int) -> Result<()> {
        let rc = unsafe { libc::ioctl(file.as_raw_fd(), request, value) };
        if rc < 0 {
            Err(ioctl_error(request))
        } else {
            Ok(())
        }
    }

    fn ioctl_none(file: &File, request: libc::c_ulong) -> Result<()> {
        let rc = unsafe { libc::ioctl(file.as_raw_fd(), request) };
        if rc < 0 {
            Err(ioctl_error(request))
        } else {
            Ok(())
        }
    }

    fn ioctl_error(request: libc::c_ulong) -> DogiError {
        DogiError::Transport(format!(
            "uinput ioctl 0x{request:x} failed: {}",
            io::Error::last_os_error()
        ))
    }

    fn as_bytes<T>(value: &T) -> &[u8] {
        unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), mem::size_of::<T>()) }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn maps_runtime_keys_to_linux_codes() {
            assert_eq!(linux_key_code(RuntimeKey::Control), KEY_LEFTCTRL);
            assert_eq!(linux_key_code(RuntimeKey::Equal), KEY_EQUAL);
            assert_eq!(linux_key_code(RuntimeKey::VolumeDown), KEY_VOLUMEDOWN);
        }

        #[test]
        fn uinput_ioctl_values_match_linux_headers() {
            assert_eq!(UI_DEV_CREATE, 0x5501);
            assert_eq!(UI_DEV_DESTROY, 0x5502);
            assert_eq!(UI_SET_EVBIT, 0x40045564);
            assert_eq!(UI_SET_KEYBIT, 0x40045565);
            assert_eq!(UI_SET_RELBIT, 0x40045566);
        }

        #[test]
        fn scales_horizontal_scroll_and_carries_fractional_remainders() {
            let mut accumulator = HorizontalScrollAccumulator::default();

            assert_eq!(
                accumulator.scale(1, 8, 1, 25),
                HorizontalScrollMovement {
                    high_resolution: 2,
                    low_resolution: 0,
                }
            );
            assert_eq!(
                accumulator.scale(1, 3, 1, 50),
                HorizontalScrollMovement {
                    high_resolution: 1,
                    low_resolution: 0,
                }
            );
            assert_eq!(
                accumulator.scale(1, 3, 1, 50),
                HorizontalScrollMovement {
                    high_resolution: 2,
                    low_resolution: 0,
                }
            );
        }

        #[test]
        fn emits_low_resolution_steps_after_120_high_resolution_units() {
            let mut accumulator = HorizontalScrollAccumulator::default();

            assert_eq!(
                accumulator.scale(15, 8, 1, 100),
                HorizontalScrollMovement {
                    high_resolution: 120,
                    low_resolution: 1,
                }
            );
            assert_eq!(
                accumulator.scale(15, 8, -1, 100),
                HorizontalScrollMovement {
                    high_resolution: -120,
                    low_resolution: -1,
                }
            );
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use super::*;

    pub struct SystemRuntimeActionExecutor;

    impl SystemRuntimeActionExecutor {
        pub fn open() -> Result<Self> {
            Err(DogiError::BackendUnavailable(
                "local runtime action execution is only implemented on Linux uinput".to_owned(),
            ))
        }
    }

    impl RuntimeActionExecutor for SystemRuntimeActionExecutor {
        fn execute_command(&mut self, _command: &RuntimeCommand) -> Result<()> {
            Err(DogiError::BackendUnavailable(
                "local runtime action execution is only implemented on Linux uinput".to_owned(),
            ))
        }
    }
}

pub use platform::SystemRuntimeActionExecutor;

#[cfg(test)]
mod tests {
    use super::*;
    use dogi_core::{RuntimeActionSource, RuntimeKey};

    #[derive(Default)]
    struct RecordingExecutor {
        commands: Vec<RuntimeCommand>,
        fail_next: bool,
    }

    impl RuntimeActionExecutor for RecordingExecutor {
        fn execute_command(&mut self, command: &RuntimeCommand) -> Result<()> {
            self.commands.push(command.clone());
            if self.fail_next {
                self.fail_next = false;
                Err(DogiError::Transport("synthetic failure".to_owned()))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn executes_supported_actions_and_reports_status() {
        let actions = vec![
            ResolvedRuntimeAction {
                source: RuntimeActionSource::ThumbWheel,
                command: RuntimeCommand::KeyChord(vec![RuntimeKey::Control, RuntimeKey::Equal]),
            },
            ResolvedRuntimeAction {
                source: RuntimeActionSource::Button(dogi_core::Master3sButton::Middle),
                command: RuntimeCommand::MouseButton(RuntimeMouseButton::Middle),
            },
        ];
        let mut executor = RecordingExecutor::default();

        let executions = execute_runtime_actions_with(&actions, &mut executor);

        assert_eq!(executions.len(), 2);
        assert!(
            executions
                .iter()
                .all(|execution| execution.status == RuntimeActionExecutionStatus::Executed)
        );
        assert_eq!(executor.commands.len(), 2);
    }

    #[test]
    fn skips_noop_and_unsupported_without_touching_executor() {
        let actions = vec![
            ResolvedRuntimeAction {
                source: RuntimeActionSource::Button(dogi_core::Master3sButton::Back),
                command: RuntimeCommand::Noop,
            },
            ResolvedRuntimeAction {
                source: RuntimeActionSource::UnknownControlId(0xbeef),
                command: RuntimeCommand::Unsupported,
            },
        ];
        let mut executor = RecordingExecutor::default();

        let executions = execute_runtime_actions_with(&actions, &mut executor);

        assert_eq!(
            executions
                .iter()
                .map(|execution| execution.status)
                .collect::<Vec<_>>(),
            vec![
                RuntimeActionExecutionStatus::Skipped,
                RuntimeActionExecutionStatus::Unsupported,
            ]
        );
        assert!(executor.commands.is_empty());
    }

    #[test]
    fn reports_executor_failures_per_action() {
        let actions = vec![ResolvedRuntimeAction {
            source: RuntimeActionSource::ThumbWheel,
            command: RuntimeCommand::KeyChord(vec![RuntimeKey::VolumeUp]),
        }];
        let mut executor = RecordingExecutor {
            fail_next: true,
            ..RecordingExecutor::default()
        };

        let executions = execute_runtime_actions_with(&actions, &mut executor);

        assert_eq!(executions[0].status, RuntimeActionExecutionStatus::Failed);
        assert!(
            executions[0]
                .detail
                .as_ref()
                .is_some_and(|detail| detail.contains("synthetic failure"))
        );
    }

    #[test]
    fn system_execution_does_not_open_uinput_for_non_actionable_commands() {
        let actions = vec![
            ResolvedRuntimeAction {
                source: RuntimeActionSource::Button(dogi_core::Master3sButton::Back),
                command: RuntimeCommand::Noop,
            },
            ResolvedRuntimeAction {
                source: RuntimeActionSource::UnknownControlId(0xbeef),
                command: RuntimeCommand::Unsupported,
            },
        ];

        let executions = execute_runtime_actions(&actions).unwrap();

        assert_eq!(
            executions
                .iter()
                .map(|execution| execution.status)
                .collect::<Vec<_>>(),
            vec![
                RuntimeActionExecutionStatus::Skipped,
                RuntimeActionExecutionStatus::Unsupported,
            ]
        );
    }
}
