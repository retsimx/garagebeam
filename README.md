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
  and `scripts/latency_report.sh`
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

## Latency measurement (GB-6)

`garage_beam` can emit one raw latency sample per accepted beam edge. The
instrumentation is **deny-by-default**: it is active only when
`GARAGE_BEAM_LATENCY` is set to a truthy value — `1`, `true`, `yes` or `on`,
compared case-insensitively. Anything else, including unset, leaves the control
path unchanged and adds no clock reads, no characteristic reads and no per-event
log lines (a single startup line still reports `enabled=false`). The gate is read
once at startup, so a restart is required to change it.

The committed OpenRC script exports only `GARAGE_BEAM_DEVICE_ADDRESS` and
`RUST_LOG`, so export `GARAGE_BEAM_LATENCY` from the environment that launches
`garage_beam`. For a bench run, prefix the command:

```sh
GARAGE_BEAM_LATENCY=1 GARAGE_BEAM_DEVICE_ADDRESS="$ADDR" ./garage_beam 2>&1 | tee latency-idle.log
```

Each accepted edge then emits a line at INFO with target `garage_beam::latency`:

```
2026-09-19T16:00:00.000000Z  INFO garage_beam::latency: latency_sample edge_to_write_ns=123456 write_to_read_ns=78901 read_back=1
```

### Segment model

| Segment | Meaning | Clock |
|---|---|---|
| (a) `edge_to_write_ns` | beam edge (kernel GPIO event timestamp) → BLE write issued | host `CLOCK_MONOTONIC` |
| (b) `write_to_read_ns` | BLE write → measurement read returned | host `CLOCK_MONOTONIC` |
| (c) total | derived `(a) + (b)` for samples that carry a `(b)` value | host |
| (d) peripheral internal | peripheral receive → lamp applied, from the peripheral's own log | peripheral monotonic |

Segment (b) is emitted only when the measurement read succeeds, so some samples
carry (a) alone. The peripheral delta (d) is a cross-repo item owned by
`retsimx/garagelight` (GL-13/#14) and is consumed as a sanity check.

### Run procedure

Measure each of the four load conditions separately, and collect at least 50
beam transitions in each:

1. **idle** — no DHT11 read, no OTA, no other traffic.
2. **during a DHT11 read**.
3. **during an OTA download**.
4. **with active WiFi traffic**.

For each condition, enable the gate, exercise the beam until at least 50
`latency_sample` lines are captured, then stop. Keep one log per condition, for
example `latency-idle.log`, `latency-dht.log`, `latency-ota.log` and
`latency-wifi.log`. Summarise a log with:

```sh
scripts/latency_report.sh latency-idle.log
```

The report prints `N`, `min`, `p50`, `p95`, `p99` and the full ascending raw
sample list for segments (a), (b) and the derived (c), so the distribution (and
any bimodality) stays visible. With no argument it reads standard input.

### Percentile method

Percentiles are **nearest-rank**: sort the samples ascending, then take rank
`ceil(p/100 * N)`, 1-based and clamped to `1..N`. There is no interpolation, so
every reported value is an observed sample.

### Limitations

- Host and peripheral clocks are unsynchronised, so segment (d) cannot be
  subtracted from the host segments; it is a same-clock check on the peripheral
  only.
- (b) is a round-trip **bound** on peripheral receive→apply, not pure
  peripheral time: it includes the read round trip.
- The beam sensor and its mechanical relay contribute a hardware floor of
  roughly 6–20 ms that is estimated, not measured in software.
- Measurement mode performs an extra read after each accepted write, so
  `(a) + (b)` is an upper bound on the production path, not the production path
  itself.
