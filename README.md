# Dogi

Dogi is a Linux desktop application for configuring Logitech mice, currently focused on the
MX Master 3S. It provides a Slint GUI, direct HID++ device access, and a local runtime for custom
actions.

## Features

- Multi-device profiles with independent drafts and settings.
- Pointer speed, scroll wheel, thumb wheel, button, and application-profile controls.
- Five-way gesture assignments with an in-app gesture test surface.
- Device identity, capability, connection, and battery detection through HID++.
- Explicit, diff-only device writes with confirmation before changes are applied.
- Custom keyboard and mouse actions through Linux `uinput`.
- System tray integration, background runtime management, and English/Chinese UI.
- Verified updates from immutable GitHub Releases with system, direct, and manual proxy modes.

## Build and run

```bash
cargo build
./target/debug/dogi
```

Running `dogi` without a command opens the GUI. Run it as the desktop user; root is only required
to install the device-access rules.

```bash
sudo ./target/debug/dogi udev install
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=hidraw
sudo udevadm trigger --subsystem-match=misc
```

Reconnect the receiver or mouse after installing the rules. Use `dogi doctor` to verify access to
`hidraw`, `uinput`, active-window detection, and the user service.

## Common commands

```bash
dogi list
dogi inspect <device-id>
dogi doctor
dogi config show
dogi config plan
dogi runtime plan
dogi service print
```

Run `dogi <command> --help` for complete options.

## Configuration safety

Saving stores the selected profile locally. Applying is a separate confirmed operation that writes
only changed HID++ setting groups. CLI writes additionally require `--allow-device-write`.

Dogi does not manage Logitech onboard-profile memory. Custom actions and non-native thumb-wheel
behavior are implemented locally through the background runtime and `uinput`.

## Configuration files

Installed and portable releases store configuration under `$XDG_CONFIG_HOME/dogi`, or
`$HOME/.config/dogi` when `XDG_CONFIG_HOME` is unset:

- `config.json` — application, update, and non-secret network preferences.
- `master3s.json` — default and per-device mouse profiles.

Configuration writes use atomic replacement. Unsupported or malformed schemas are reported and
left unchanged. Authenticated proxy passwords are stored in the desktop keyring, never in
`config.json`.

Local Cargo builds, including `--release` builds, use an isolated `dogi-development` namespace for
configuration, cache, runtime sockets, and the GUI instance lock. They never install or rewrite the
release background service, and automatic updates are disabled. To exercise runtime actions during
development, start the foreground runtime explicitly in another terminal:

```bash
cargo run -- runtime run --execute-actions --allow-device-write
```

Only one action runtime can own `uinput` at a time. Stop the installed Dogi runtime before starting
the development runtime; Dogi reports the conflict instead of taking it over.

## Desktop integration

The Debian package ships a fixed systemd user unit that runs `/usr/bin/dogi`; the GUI only enables,
disables, or restarts it. Portable installations create their user unit only after background
operations are explicitly enabled. Unmanaged and development binaries do not create persistent
services.

X11 application matching uses `xprop`. Wayland does not expose an equivalent global active-window
API, so profiles can be edited there but automatic profile switching remains unavailable.
