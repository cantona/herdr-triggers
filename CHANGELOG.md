# Changelog

What changed in each release of herdr-triggers. Every release so far is a
prerelease.

## [0.2.2] - 2026-11-17

### Changed
- Licensed under the Apache License 2.0 instead of MIT; every source file
  carries `SPDX-FileCopyrightText` and `SPDX-License-Identifier: Apache-2.0`
  lines (the `.rs`, `.py`, `.sh` and `.js` files). Releases up to 0.2.1
  remain available under MIT.
- The README is now an overview with a benchmarks table; the full reference
  moved to `docs/GUIDE.md`.

### Fixed
- The docs said a `coprocess` is fed the screen. It is only with
  `feed_screen = true` (its last 48 KiB); the guide now says so.

## [0.2.1] - 2026-11-09

### Added
- `herdr-triggersd install` sets up the Herdr desktop app in one step: the
  dialog mod (built into the binary), its `keys.json` bindings, a link from
  the app's config to the TUI plugin's, and a login service (a systemd user
  unit on Linux, a launchd agent on macOS) running `--host core service`. It
  prints the plan and asks first; `uninstall` removes it again. `keys.json`
  is never created and keeps its key order.
- `--host core service` stays in the foreground for a service manager: it
  waits for herdr-core's socket, watches its terminals while core answers,
  and waits again when core goes away. It always watches the default
  session.

### Changed
- `stop` and `start` for the default core go through the service manager,
  which restarts the daemon after any other exit; `start` falls back to a
  detached daemon when that fails. Programs started by `run` rules survive a
  service stop or restart.
- The app's sidebar block no longer starts the daemon; it only shows status.

## [0.2.0] - 2026-11-08

### Added
- `--host core` watches the Herdr desktop app's terminals (herdr-core). One
  subscription keeps the terminal table current from pushed events, so a
  poll costs one read per watched pane. Typing goes to the terminal as raw
  bytes. A missing or failing desktop notifier keeps a rule armed, so a
  `once` rule is not spent on a notice nobody saw.
- A core daemon exits after 30 s of failed reconnects to core
  (`HERDR_TRIGGERS_CORE_PATIENCE_MS`); with `--follow-app` it also exits
  once the Herdr app has refused connections for 10 s.
- `status --json`, with the last fire read from the log.
- A Herdr desktop app dialog and sidebar block (`mods/herdr-triggers`) to
  see the daemon's state and start, stop or restart it. The sidebar block
  starts the core daemon with `--follow-app`.
- Desktop notifications on macOS, through `osascript`.

### Changed
- `stop` now returns only once the daemon has exited, and `start` waits for
  a daemon that is stopping, each for up to 30 s, since a stop lands between
  rules.
- Log wording: "cannot reach herdr" is now "cannot reach the server", and a
  missing `notify-send` no longer says "(Linux only)".

### Fixed
- Log rotation no longer kills the daemon when the kept tail starts inside a
  multi-byte character.

## [0.1.1] - 2026-09-18

### Changed
- The plugin id is now `cantona.herdr-triggers`, so its config and state
  directories move to `plugins/config/cantona.herdr-triggers` (and the state
  equivalent). Move `triggers.toml` and `secrets.toml` across after
  upgrading.
- Polling makes fewer requests and repeats less matching as panes, rules and
  screens grow; `scripts/benchmark.py` measures it.

### Fixed
- A failed or malformed screen read no longer re-arms a trigger whose prompt
  is still there; only a successful read of a blank screen clears it.

## [0.1.0] - 2026-09-01

### Added
- Regex triggers over herdr pane output, written in `triggers.toml`: a
  pattern, optionally aimed at particular panes, and an action when it
  matches. The action types text into the pane, runs a subprocess, types
  back a coprocess's reply (optionally feeding it the screen), raises a
  notification, or marks a tab.
- One daemon per herdr server, started by the plugin's startup hook.
- `${secret:NAME}` references, resolved from `secrets.toml`, so rules never
  carry a secret in plain text.

[0.2.2]: https://github.com/cantona/herdr-triggers/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/cantona/herdr-triggers/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/cantona/herdr-triggers/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/cantona/herdr-triggers/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/cantona/herdr-triggers/releases/tag/v0.1.0
