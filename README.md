# Hearth

A terminal workspace for Linux where a per-user server owns your shells.
Close the window, reconnect later (locally, over SSH, or over your
tailnet), and the same programs are still running, with exact screen
state.

Status: the server and CLI work and are tested. The GTK desktop client is
in progress. See [docs/PARITY.md](docs/PARITY.md) for what has been
verified and what has not.

## Build

Requires Ubuntu 26.04 and Rust 1.94 or newer.

```sh
cargo build --release
cargo build && cargo test      # e2e tests use the built binaries
```

## Use

```sh
hearth new -n work            # starts the server if needed, new session
hearth ls
hearth split -t b2 -d right -- htop
hearth run -p window --wait -- make test
hearth attach -t b2           # view a terminal here; Ctrl-] detaches
hearth capture -t b2 --history
hearth --host ssh:me@box ls   # another machine (needs hearth installed there)
```

Inside a Hearth terminal, `$HEARTH_SESSION` and `$HEARTH_BLOCK` are set and
commands target them by default.

Configuration goes in `~/.config/hearth/config.lua`:

```lua
hearth.bind("ctrl+shift+t", "new-tab")
hearth.host{ name = "box", address = "ssh:me@box" }
hearth.action{ name = "dev", title = "Dev layout",
  args = { { name = "dir", type = "string", required = true } },
  run = function(a)
    local s = hearth.api.session_create{ name = "dev", cwd = a.dir }
    hearth.api.block_split{ block = s.block, direction = "right", cwd = a.dir, command = { "npm", "run", "dev" } }
  end }
-- opt-in tailnet listener; identity via `tailscale whois`
-- hearth.set{ listen = { "tcp:100.64.0.1:7701" }, allow = { "me@example.com" } }
```

Further reading:

- [docs/DECISIONS.md](docs/DECISIONS.md): architecture decisions.
- [docs/PROTOCOL.md](docs/PROTOCOL.md): the wire protocol.

## License

MIT. Dependencies keep their own licenses: alacritty_terminal is Apache-2.0,
Lua is MIT, and GTK/VTE are LGPL.
