pub(crate) mod actions;
pub(crate) mod control;
pub(crate) mod service;

pub(crate) const UINPUT_PATH: &str = "/dev/uinput";

pub(crate) use actions::{
    RuntimeActionExecution, SystemRuntimeActionExecutor, execute_runtime_actions,
    execute_runtime_actions_with,
};
