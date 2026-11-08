# Changelog

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.1.2] - 2026-11-09

### Added

- CI, contributing guide, issue templates, this changelog and a demo GIF.

### Removed

- Claude status hooks. Claude Code reports its own state over OSC 7501. `hearth agent setup` removes the hooks an earlier version installed.

## [0.1.1] - 2026-11-08

### Added

- Claude's state shows on its tab, and notifications name the agent.

## [0.1.0] - 2026-11-07

First version.

### Added

- `hearthd` per-user server, `hearth` CLI and the `hearth.v1` protocol.
- GTK desktop app, `.deb` packaging and GUI tests.
- Tailnet listener that refuses connections from other local accounts.
- Snapshot tests against output recorded from real programs.
- A log line for every close, with what was closed and who asked.

### Fixed

- One terminal failing no longer takes the server down.
- Closed terminals release their threads, files and memory.
- The app only wakes when there is work.

[Unreleased]: https://github.com/jayhemnani9910/hearth/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/jayhemnani9910/hearth/releases/tag/v0.1.2
