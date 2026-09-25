# Dogi

Dogi is a Linux desktop application for configuring Logitech mice, with a focus on the
MX Master 3S. It brings device settings, custom actions, and battery monitoring together in a
native desktop interface.

## Features

### Mouse settings

- Adjust pointer speed, scroll wheel behavior, and thumb wheel scrolling speed.
- Assign button actions, keyboard shortcuts, and five-way gestures.
- Test thumb wheel scrolling and gesture assignments directly in the app.

### Devices and profiles

- Keep independent settings and unsaved changes for each device.
- Create application-specific profiles, with automatic switching on X11.
- View device identity, connection state, available capabilities, and battery status.
- Receive desktop notifications for low battery and a full charge.

### Desktop integration

- Run in the system tray and manage background actions from the app.
- Choose an English or Chinese interface.
- Check for and install updates, with system, direct, or custom proxy settings.

## Settings and safety

Saving a profile and applying device settings are separate operations. Dogi previews pending
changes and requests confirmation before applying only the modified device settings.

Custom actions and software-controlled thumb wheel behavior run locally through the background
runtime. Dogi does not update device firmware or manage onboard-profile memory.

## Compatibility

Dogi currently targets the MX Master 3S on Linux. Available controls depend on the connected
device's capabilities and access permissions; support for other Logitech models is not guaranteed.

On Wayland, application profiles can be edited, but automatic application-profile switching is
currently unavailable. Device configuration and software actions require access to the relevant
HID and input interfaces.
