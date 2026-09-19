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

## Testing and CI

The project follows one rule: **policy is tested on the host in CI; hardware is
tested on the bench.** Pure decisions — beam debounce, write scheduling, and
reconnect policy — run as ordinary `cargo test` cases with no hardware attached,
so the host suite passes on any machine.

CI runs the same checks on every push and pull request:

- `cargo fmt --all --check` and `cargo clippy --all-targets -- -D warnings`
- the host `cargo test` suite, hardware-free
- the ARM release cross build (`arm-unknown-linux-musleabihf`)
- `shellcheck` over the OpenRC service script (`deploy.sh` too, when present)
- a cross-repo byte-equality check of `contract.toml` against the sibling firmware

See [`.github/workflows/ci.yml`](.github/workflows/ci.yml) for the exact steps.
Radio and timing behaviour is verified on the bench rather than in CI.

## Service

An OpenRC init script is provided (`garage_beam.openrc`). Adjust to `/etc/init.d/garage_beam`.

## Operations

### Service relationship

`garage_beam` and `rshunterbtt` are independent OpenRC services. Restarting
either must leave the other running.

A historical outage in which restarting `garage_beam` also stopped `rshunterbtt`
was diagnosed to the deployed host `/etc/init.d/rshunterbtt`, which declared
`need dbus bluetooth garage_beam`. OpenRC `need` is a strong dependency: stopping
the needed service stops its dependents, and `rc-service -Z garage_beam restart`
proved the cascade. The deployed script has since been restored to its committed
upstream form, and `garage_beam` now has its own `default` runlevel entry.

Do not add any cross-service declaration to `garage_beam.openrc`; use
`use`/`after` for ordering only.

### Single clone

Exactly one authoritative checkout of this repository — the repository working
tree — builds every deployed binary. Deployed binaries are built from that clone
only, and duplicate checkouts are retired.

### Environment variables

| Variable | Required | Default | Source |
|---|---|---|---|
| `GARAGE_BEAM_DEVICE_ADDRESS` | yes | none | `/etc/conf.d/garage_beam` (untracked host config) |
| `RUST_LOG` | no | `garage_beam=info,warn` | operator environment or `/etc/conf.d/garage_beam` |

- `GARAGE_BEAM_DEVICE_ADDRESS` is the lamp peripheral's BLE address. It has no
  committed default because this repository is public; the value lives only in
  the untracked host config.
- `RUST_LOG` defaults to `garage_beam=info,warn`, which keeps steady state
  silent; an operator can raise it when diagnosing.

`garage_beam.openrc` sources `/etc/conf.d/garage_beam` when present and lets it
override the command and working directory.

### Host LE connection interval

Keep `[LE] MinConnectionInterval=6` and `MaxConnectionInterval=6` in
`/etc/bluetooth/main.conf`. The host-wide value makes every new LE connection
start at the 7.5 ms minimum, so the lamp link comes up fast before the explicit
`LE Connection Update` lands; the sprinkler bridge `rshunterbtt` requests and
enforces its own interval at runtime and is unaffected. Do not remove this value
as "cleanup" — see design doc 006 (host LE interval).
