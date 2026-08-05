use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{Command, Output};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use dogi_core::{DogiError, Result};

#[derive(Clone, Debug)]
pub(crate) struct UserContext {
    pub(crate) home: PathBuf,
    pub(crate) uid: Option<u32>,
    pub(crate) gid: Option<u32>,
}

impl UserContext {
    pub(crate) fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        let mut command = Command::new(program);
        command
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .args(args);

        if let Some(uid) = self.uid {
            command
                .env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"))
                .env(
                    "DBUS_SESSION_BUS_ADDRESS",
                    format!("unix:path=/run/user/{uid}/bus"),
                );
        }

        #[cfg(unix)]
        if let (Some(uid), Some(gid)) = (self.uid, self.gid) {
            command.uid(uid).gid(gid);
        }

        command.output()
    }
}

pub(crate) fn current_user() -> Result<UserContext> {
    if let Some(context) = elevated_user() {
        return Ok(context);
    }

    let home = env_path("HOME")
        .ok_or_else(|| DogiError::Config("HOME is not set for desktop integration".to_owned()))?;
    Ok(UserContext {
        home,
        uid: None,
        gid: None,
    })
}

pub(crate) fn env_path(key: &str) -> Option<PathBuf> {
    env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(unix)]
pub(crate) fn elevated_user() -> Option<UserContext> {
    if unsafe { libc::geteuid() } != 0 {
        return None;
    }

    let uid = env::var("SUDO_UID").ok()?.parse::<u32>().ok()?;
    let gid = env::var("SUDO_GID").ok()?.parse::<u32>().ok()?;
    if uid == 0 {
        return None;
    }

    let home = fs::read_to_string("/etc/passwd")
        .ok()?
        .lines()
        .find_map(|line| {
            let mut fields = line.split(':');
            let _name = fields.next()?;
            let _password = fields.next()?;
            let candidate_uid = fields.next()?.parse::<u32>().ok()?;
            let _candidate_gid = fields.next()?;
            let _gecos = fields.next()?;
            let home = fields.next()?;
            (candidate_uid == uid && !home.is_empty()).then(|| PathBuf::from(home))
        })?;

    Some(UserContext {
        home,
        uid: Some(uid),
        gid: Some(gid),
    })
}

#[cfg(not(unix))]
pub(crate) fn elevated_user() -> Option<UserContext> {
    None
}
