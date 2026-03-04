# Garage Beam BLE Bridge

Garage Beam is an incredibly lightweight Rust service created to run on an embedded Raspberry Pi Zero W (Gen 1) traversing Alpine Linux. It functions as a bridge that constantly reads the state of a local GPIO pin, and mirrors that physical state asynchronously to a predetermined Bluetooth Low Energy (BLE) device. 

## Features

- **Asynchronous GPIO Reading**: Reads Sysfs GPIO state efficiently.
- **BLE Integration**: Implemented heavily utilizing `btleplug`.
- **D-Bus Managed**: Designed to interface smoothly in systems (especially Alpine openRC systems).
- **Comprehensive Testing**: Tested via internal components utilizing simulated mocks (`mockall`).

## Requirements

Ensure the target system includes D-Bus and BlueZ:

```sh
apk add --no-cache dbus bluez
```

Ensure the services are running:

```sh
rc-update add dbus
rc-update add bluetooth
```

## Cross Compilation

This project configures itself to build across to `arm-unknown-linux-musleabihf` for lightweight execution on an Alpine stack.

Use [cross](https://github.com/cross-rs/cross):

```sh
cross build --target arm-unknown-linux-musleabihf --release
```

## Service

An OpenRC init script is provided (`garage_beam.openrc`). Adjust to `/etc/init.d/garage_beam`.
