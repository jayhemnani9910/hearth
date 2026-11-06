# Parity ledger

Status is one of: `verified` (an automated test passed on Ubuntu 26.04),
`implemented` (code exists but the required evidence is missing),
`unimplemented`, `different` or `unknown`. Test names refer to
`crates/hearthd/tests/e2e.rs` unless noted. Nothing here was compared
against a real Rex installation.

| ID | Behavior | Status | Evidence / gap |
| --- | --- | --- | --- |
| P01 | Open locally, no config/accounts | implemented | `p01_cli_autostarts_a_personal_server_offline` (CLI path). **GUI pending** (GTK/VTE dev packages missing) |
| P02 | Server owns programs | verified (CLI/protocol clients) | `p02_client_exit_and_crash_keep_the_same_process`: same PID + start time after clean exit and SIGKILL of a client. GUI client not yet built |
| P03 | Terminal state survives reattach | verified (server model) | `model_tests.rs`: split at every byte offset (UTF-8, CSI/OSC/DCS, alt screen, regions, origin, saved cursor, modes, wide/combining, links). e2e: `p03_repeated_attach_during_output_is_exact`, `p03_alternate_screen…`, `p03_detached_output_is_drained`, `p03_server_is_the_only_query_responder`. **Not yet checked against VTE** |
| P04 | Sessions and tabs | verified (API) | `p04_p05_structure_ids_and_layout`: stable IDs, rename, reorder, ambiguous-name refusal, no ID reuse |
| P05 | Split panes | verified (API) | same test: nested splits, resize, balance, zoom, swap, move. Divider drag UI pending |
| P06 | Independent interaction | implemented | Server keeps no per-viewer scroll/selection; detaching B leaves A unaffected (`p06_p07…`). GUI scroll/selection test pending |
| P07 | Geometry ownership | verified | `p06_p07_size_ownership_and_independent_viewers`: owner-only resize, ping-pong bounded, typing transfers, disconnect hands back |
| P08 | Multiple hosts | verified (two local servers) | `p08_two_servers_route_to_the_right_host_via_proxy_transport` (proxy transport). Real ssh to another machine: **untested** |
| P09 | Tailnet access | implemented | `p09_tcp_identity_policy_with_stub_whois`: allow/deny/empty/whois-failure policy, using a **stub** whois. Real Tailscale not installed: **unverified** |
| P10 | Sleep/network reconnect | implemented | `resume_within_window_and_fresh_attach_after`, `slow_viewer_is_cut_loose…`. Two-machine sleep/network switch: **untested** |
| P11 | CLI and JSON | verified | `p11_p13_cli_json_capture_wait_and_exit_codes`, `p04_p05…` (`ls --json`) |
| P12 | Introspection | verified | `p12_every_described_method_exists_and_validates` |
| P13 | Capture and events | verified | text/html/vt capture; `p13_events_are_ordered_and_bootstrap_is_atomic` |
| P14 | Lua actions/config | verified (server side) | `p14_lua_config_actions_reload_and_errors`, `p14_lua_scripts_drive_a_live_server_with_events`. Key sequences/modes are parsed and served; **client-side execution pending GUI** |
| P15 | Agent integration | verified | `p15_agent_setup_is_idempotent_and_reversible`, `p15_cli_inside_a_terminal_targets_its_own_block` |
| P16 | Program status | verified (server) | `osc7501.rs` unit tests, `p16_osc7501_records_and_prompt_lifetime`. UI indicators/notifications pending |
| P17 | Native UI polish | unimplemented | blocked on GTK/VTE dev packages |
| P18 | Advanced terminal features | unknown | truecolor/links/mouse modes are carried in snapshots; inline graphics are not supported |
| P19 | Service upgrades | unimplemented | systemd user unit and upgrade test pending (phase 6) |
| P20 | Distribution | unimplemented | .deb packaging pending (phase 6) |

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
- No shared/login server modes, no PAM gateway, no embedded Tailscale, no
  floating layers, no block plugin API.
- No survival guarantee across logout: it depends on systemd lingering and
  is untested.
- Another OS user being denied by `SO_PEERCRED` is implemented but untested,
  because no second account is available.
