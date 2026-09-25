#![cfg_attr(
    not(test),
    deny(clippy::expect_used, clippy::panic, clippy::unwrap_used)
)]

mod application;
mod cli;
mod config;
mod desktop;
mod device;
mod domain;
mod environment;
mod hid;
mod network;
mod persistence;
mod runtime;
mod ui;
mod update;

fn main() -> std::process::ExitCode {
    if let Some(exit_code) = update::run_internal_command() {
        exit_code
    } else {
        cli::run()
    }
}
