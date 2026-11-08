# Contributing

I try to reply to issues within a day or two. Questions and ideas go in
[Discussions](https://github.com/jayhemnani9910/hearth/discussions).

## Setup

On Ubuntu 26.04, with Rust 1.94 or newer:

```
sudo apt install libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev libgtk-4-bin
cargo build
```

## Tests

```
cargo build && cargo test --locked
```

The tests use the built binaries, so build first. The GUI tests run the real
app on a headless GTK Broadway display. Each test gets its own server, socket
and config, so they don't touch a Hearth you have running.

To try a change by hand without touching your own server, point it at its own
directories:

```
export HEARTH_RUNTIME_DIR=/run/user/$(id -u)/hearth-dev
export HEARTH_STATE_DIR=/tmp/hearth-dev/state HEARTH_CONFIG_DIR=/tmp/hearth-dev/cfg
target/debug/hearth-app
```

## Before you open a PR

- Add a test for the behavior, through the CLI or the app if you can.
- If you change what's verified, update [docs/PARITY.md](docs/PARITY.md). Only
  mark something `verified` if a test proves it.
- If you change the wire format, update [docs/PROTOCOL.md](docs/PROTOCOL.md).
- Add a line to [CHANGELOG.md](CHANGELOG.md) under Unreleased.

Good places to start are the
[good first issues](https://github.com/jayhemnani9910/hearth/labels/good%20first%20issue)
and anything marked untested in the parity ledger.
