# podman-healthcheckd

A lightweight Rust daemon that schedules Podman healthchecks without relying on systemd timers. Works on any Linux distribution.

## Why

Podman normally depends on systemd timers to run container healthchecks. On systems without systemd (Guix, Alpine, Void, etc.), healthchecks simply never execute. This daemon replaces that functionality with a standalone process.

## How it works

1. On startup, enumerates running containers and inspects each for a healthcheck config
2. Spawns an async timer per container that has a healthcheck defined
3. Watches `podman events` for container start/stop/remove events and adjusts timers accordingly
4. On SIGINT/SIGTERM, cancels all timers and exits cleanly

The daemon only acts as a clock — `podman healthcheck run` handles retries, failing streaks, and on-failure actions internally.

## Build

```bash
cargo build --release
```

On Guix:

```bash
guix shell rust rust:cargo gcc-toolchain -- sh -c "CC=gcc cargo build --release"
```

## Usage

```bash
RUST_LOG=info ./target/release/podman-healthcheckd
```

Log levels: `error`, `warn`, `info`, `debug`, `trace` (via `RUST_LOG`).

## License

MIT
