# Architecture decisions

## D1. Supported platform

Ubuntu 26.04 LTS (resolute), amd64, GNOME on Wayland. This is the only
configuration the project is built and tested on. Other Linux distributions
and X11 sessions are expected to work but are untested.

## D2. Language and terminal engine

- **Rust** for the server, CLI, and desktop client (toolchain 1.94).
- **Server terminal model: `alacritty_terminal` 0.26 (Apache-2.0).** It keeps
  both screens, the history and the modes, and it answers terminal queries
  (DA, DSR/CPR, DECRQM, kitty keyboard, colors), which makes it the single
  source of replies.
- **Desktop terminal widget: VTE (GTK4, LGPL-3.0)** via the `vte4` crate,
  inside a GTK4 + libadwaita application.

Rejected for now:

- The Ghostty fork / libghostty-vt path. It needs Zig 0.17 (not installed),
  its snapshot format is marked unstable, and on GTK a custom renderer would
  have to be written.
- A webview client, which changes the native experience.

Consequence: the snapshot format is **our own VT-sequence serialization**
(see `crates/hearthd/src/model.rs`), not the Ghostty `GHOSTSNP` codec. It is
restored by feeding bytes to any VT emulator. Fidelity is enforced by tests
that restore a snapshot into a fresh model, continue both with the same
bytes, and require identical state. The tests split at every byte offset.

## D3. Process and ownership model

- `hearthd` is a per-user server: one process per user. It owns sessions,
  windows (tabs), split layouts and blocks.
- A block owns a PTY, its child process, the canonical model, and a 4 MiB
  output ring.
- PTY output is drained with zero viewers attached.
- Clients only view and send input. Closing or killing a client never closes
  a PTY.
- Children get `setsid` and a controlling TTY, and are reaped by a waiter
  thread.
- Killing a block sends `SIGHUP` to its process group, then `SIGKILL` after
  3 s.

## D4. Snapshot / stream boundary

- Output enters one ordered stream per block. A byte segmenter holds back
  unfinished UTF-8 characters and escape sequences.
- The model only consumes complete pieces, so a snapshot is exact at
  `applied()`.
- Attach happens under the block lock and does three things in order:
  1. sends `Snapshot(offset=S)`;
  2. sends the ring bytes from `S` to the end;
  3. registers the viewer.
- Nothing can interleave with those steps, so every byte after `S` is
  delivered exactly once.
- The primary screen is cached when the alternate screen is entered, because
  the primary cannot change while the alternate is active.

## D5. Reply authority

The server model answers queries. Clients strip query sequences before they
reach their local emulator (`hearth_core::scan::QueryFilter`), so an
attached client can never send a second answer.

## D6. Geometry ownership

- One PTY has one size.
- The owner's preferred size wins. A client becomes owner when it first
  reports a size for an unowned block, when it types, or through
  `block.own take`.
- Resize requests from other viewers are recorded but not applied, so
  ping-pong cannot happen.
- When the owner disconnects, ownership passes to another viewer that has a
  preference.
- Resize notices travel in stream order with the terminal data.

## D7. Protocol

- `hearth.v1` is our own protocol: length-prefixed frames carrying JSON
  control messages plus typed binary data, snapshot and input frames, over a
  single stream (see `PROTOCOL.md`).
- It is not wire-compatible with Rex.
- One stream works over a Unix socket, over TCP, and over `ssh host hearth
  proxy`.

## D8. Remoting

- **Local:** Unix socket in `$XDG_RUNTIME_DIR/hearth` (directory 0700,
  socket 0600). The peer uid is checked with `SO_PEERCRED`.
- **SSH:** the client runs `ssh -T host hearth proxy`, so SSH is the
  authentication.
- **Tailnet:** an opt-in TCP listener (`hearth.set{listen=…, allow=…}`).
  - Identity comes from `tailscale whois` for the peer address, never from
    anything the peer sends.
  - An empty allow list admits nobody.
  - Embedded Tailscale is not implemented.

## D9. Persistence

- Layout is saved atomically to `$XDG_STATE_HOME/hearth/state.json`
  (schema 1), and the previous version is kept as `state.prev.json`.
- After a server restart the layout comes back with **new** shells in the
  saved directories, labelled `restored_from`. Commands are not re-run.
- A state file with a newer schema is neither loaded nor overwritten.
- The guarantee for running programs is that they survive client exit and
  disconnects while the server lives. Programs do not survive a server
  restart.

## D10. Configuration and automation

- `config.lua` (Lua 5.4, vendored through `mlua`) declares bindings, modes,
  actions, hosts and settings. Live API calls are refused while it loads.
- Action callbacks run on a dedicated Lua thread with an instruction-count
  timeout.
- The API registry (`crates/hearthd/src/api.rs`) is the single source for
  dispatch, self-description, schema validation and Lua bindings.

## D11. Desktop client

- One app window holds one connection to one host and shows one session.
  Tabs are server windows; splits are `GtkPaned`s built from the layout tree.
  Pane widgets are kept and reparented when the layout changes.
- The window reacts to `session.changed`; it does not keep its own copy of
  the layout. Divider drags are sent as ratios (`layout.set_ratio`).
- A pane attaches once it is laid out, so the attach carries its real size.
  A snapshot is fed into a VTE grid of the snapshot's size; if the widget is
  a different size, VTE then resizes it like any terminal.
- Query sequences are stripped before bytes reach VTE (D5); keystrokes leave
  through VTE's `commit` signal in order.
- On link loss the window shows a banner, reconnects with backoff and resumes
  each pane from its offset. Input that was not acknowledged is reported on
  the pane and is not resent.
- Remote control (`client.action`) is off unless the app is started with
  `--remote-control` or it is enabled from the menu.

## D12. Packaging

- A `.deb` built with `dpkg-deb` (`packaging/build-deb.sh`). Library
  dependencies come from `dpkg-shlibdeps`.
- It ships a systemd **user** unit, `hearth.service`. The clients start it
  on demand; it is not enabled globally. The maintainer scripts do not touch
  user sessions; after an upgrade they print how to restart.
- Restarting the server ends its programs; the layout comes back (D9).

## D13. GUI tests

- The real app runs on a headless GTK Broadway display against a real
  server.
- Tests drive it with `client.action`. They read its state, including VTE
  text, from a JSON probe file.
- The probe and the `test-*` actions exist only when `HEARTH_APP_PROBE` is
  set.

## Dependencies (pinned in Cargo.lock)

| Crate | Why | License |
| --- | --- | --- |
| alacritty_terminal 0.26 | server terminal model | Apache-2.0 |
| mlua 0.11 (lua54, vendored) | config/actions/scripts | MIT (Lua: MIT) |
| serde, serde_json (preserve_order in CLI) | protocol, settings edits that keep key order | MIT/Apache-2.0 |
| base64 | OSC 7501 text | MIT/Apache-2.0 |
| libc | PTY, signals, credentials | MIT/Apache-2.0 |
| gtk4 0.11, libadwaita 0.9, vte4 0.10 | desktop app | MIT bindings; GTK/libadwaita/VTE LGPL |
