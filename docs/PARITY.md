# Parity ledger

Status is one of: `verified` (an automated test passed on Ubuntu 26.04),
`implemented` (code exists but the required evidence is missing),
`unimplemented`, `different` or `unknown`. Test names refer to
`crates/hearthd/tests/e2e.rs` unless noted. Nothing here was compared
against a real Rex installation.

| ID | Behavior | Status | Evidence / gap |
| --- | --- | --- | --- |
| P01 | Open locally, no config/accounts | verified | CLI: `p01_cli_autostarts_a_personal_server_offline`. GUI: `gui_p01_first_launch_starts_a_server_and_gives_a_working_terminal` (no server, no config: the app starts one, makes a session, typing works) |
| P02 | Server owns programs | verified | `p02_client_exit_and_crash_keep_the_same_process`; GUI: `gui_p02_p03_…` kills the app (SIGKILL) and checks the same PID keeps running |
| P03 | Terminal state survives reattach | verified (text) | Server model: `model_tests.rs`, split at every byte offset. VTE: `gui_p02_p03_…` compares the VTE widget's visible text with the server's after the app restarts (colors, wide chars, links, alternate screen, return to primary) and `gui_p10_…` after a reconnect. **Colors and attributes in VTE are not compared**, only text |
| P04 | Sessions and tabs | verified | API: `p04_p05_structure_ids_and_layout`. GUI: `gui_p04_p05_…`: new tab, tab order/rename/close made by another client show up |
| P05 | Split panes | verified | API test above. GUI: `gui_p04_p05_…`: splits, PTY size = widget size, divider drag sent as a ratio, ratio set elsewhere moves the divider, zoom. The drag is driven through the divider's position (same signal path as a mouse drag), not real pointer events |
| P06 | Independent interaction | implemented | Scrollback and selection live in each client's own VTE; the server keeps none per viewer (`p06_p07…`). **No test with two apps scrolling/selecting at once** |
| P07 | Geometry ownership | verified | `p06_p07_size_ownership_and_independent_viewers`; GUI: `gui_p07_…` (another viewer takes the size, the pane says so, typing takes it back) |
| P08 | Multiple hosts | verified (two local servers) | `p08_two_servers_route_to_the_right_host_via_proxy_transport`; the app's `--host` with the proxy transport is used by `gui_p10_…`. Host switching from the session menu: **untested**. Real ssh to another machine: **untested** |
| P09 | Tailnet access | implemented | `p09_tcp_identity_policy_with_stub_whois` with a **stub** whois. A connection from this machine's own address must come from our own uid (`/proc/net/tcp` lookup; unit-tested with fixtures, own-uid path exercised by `p09`; **no second account to test a real refusal**). Real Tailscale not installed: **unverified** |
| P10 | Sleep/network reconnect | verified (link loss) | `resume_within_window_and_fresh_attach_after`, `slow_viewer_is_cut_loose…`; GUI: `gui_p10_…` cuts the app's link: banner, automatic reconnect, output made meanwhile arrives by resume, unacknowledged input is reported on the pane and not replayed. Laptop sleep / network switch between two machines: **untested** |
| P11 | CLI and JSON | verified | `p11_p13_cli_json_capture_wait_and_exit_codes`, `p04_p05…` (`ls --json`) |
| P12 | Introspection | verified | `p12_every_described_method_exists_and_validates` |
| P13 | Capture and events | verified | text/html/vt capture; `p13_events_are_ordered_and_bootstrap_is_atomic` |
| P14 | Lua actions/config | verified | Server: `p14_lua_config_actions_reload_and_errors`, `p14_lua_scripts_drive_a_live_server_with_events`. App: `gui_p14_…`: chord sequences (pending prefix shown), exclusive mode, config action on a key, palette filter and run, action with typed args, failed reload keeps bindings; keymap unit tests. Keys are injected at the keymap (`test-key`), **not as GDK key events**; the argument form dialog is **untested** |
| P15 | Agent integration | verified | `p15_agent_setup_is_idempotent_and_reversible`, `p15_cli_inside_a_terminal_targets_its_own_block` |
| P16 | Program status | verified | `osc7501.rs` unit tests, `p16_osc7501_records_and_prompt_lifetime`; GUI: `gui_p16_…`: tab spinner/indicator, pane status, bell marks a hidden tab, notification sent for a hidden blocked pane, OSC 52 reaches the clipboard. Whether GNOME displays the notification, and clicking it, is **untested** (Broadway has no notification daemon) |
| P17 | Native UI polish | implemented | libadwaita header bar, tab bar, command palette, toasts, reconnect banner, context menu, light/dark terminal colors, desktop entry and icon. Tested only on GTK Broadway (headless). **Not reviewed on a GNOME Wayland or X11 session, no accessibility review** |
| P18 | Advanced terminal features | implemented | VTE renders truecolor, OSC 8 links (Ctrl+click), mouse modes; snapshots carry them. Inline graphics are not supported. No dedicated test |
| P19 | Service upgrades | verified (simulated) | `packaging/test-package.sh`: binaries replaced the way dpkg does while a packaged server runs; programs keep running, the new CLI talks to the old server, a restart brings the layout back with new processes; `server_stop_ends_programs_but_keeps_the_layout`. A real `apt` install/upgrade (needs root) has **not been run** |
| P20 | Distribution | implemented | `packaging/build-deb.sh` builds `dist/hearth_<version>_amd64.deb` (deps from `dpkg-shlibdeps`, user unit, desktop entry, icon, license notices); `test-package.sh` checks it. **Not installed with apt on this machine** |

GUI tests run the real `hearth-app` on a headless GTK Broadway display
(`crates/hearth-app/tests/gui.rs`). They drive it through remote control and
read its state, including the VTE widgets' text, from a probe file. That
probe and the `test-*` actions exist only when `HEARTH_APP_PROBE` is set.

## Known differences and gaps (not hidden)

- **Different:** the protocol is our own (`hearth.v1`); there is no Rex wire
  compatibility.
- **Different:** snapshots are VT-sequence serialization, not Ghostty
  `GHOSTSNP`. History is sent before first paint; there is no two-step
  READY/history load.
- **Not restored on attach:** tab stops, character sets (G0–G3), the
  saved-cursor origin-mode flag, the title stack, and the kitty keyboard
  mode *stack* (only the current flags).
- **Approximation:** after a PTY resize, the emulators of viewers that are
  not the owner reflow independently. Re-attaching gives exact state again.
- The app attaches a pane the first time it is shown, so panes in tabs that
  were never opened receive no output (their status, bell and exit events
  still arrive).
- No shared/login server modes, no PAM gateway, no embedded Tailscale, no
  floating layers, no block plugin API.
- No survival guarantee across logout: it depends on systemd lingering and
  is untested.
- Another OS user being denied by `SO_PEERCRED` is implemented but untested,
  because no second account is available.
