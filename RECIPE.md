# Herdr Triggers

Regex triggers over your terminals, inside the Herdr desktop app. The same
`herdr-triggersd` that runs as a herdr TUI plugin watches herdr-core's
terminals: a prompt matches, and the rule types the answer, posts a desktop
notification, runs a script or marks the tab. One `triggers.toml` serves both.

A dialog, opened with `ctrl-alt-g` (`cmd-alt-g` on macOS), shows the daemon
and runs its commands. The daemon itself is a login service that waits for
herdr-core and follows it; an optional sidebar block shows its state.

![Herdr Triggers](preview.png)

## Install

```sh
cargo install --locked --git https://github.com/cantona/herdr-triggers
herdr-triggersd install          # shows the plan, asks y/N
```

`install` prints one line per item (`create`, `update`, `unchanged`,
`enable` or `start` for a service file whose service is not enabled or not
running, or `skip` with the reason in the last column), then asks
`Apply? [y/N]` on stderr. An `update` or `remove` of a mod file says so
when local edits there would be lost. `--yes` applies without asking,
`--dry-run` only prints. Without a terminal on both stdin and stderr, and
without `--yes`, it prints the plan and exits 2. Run it as your user: as
root with someone else's `HOME` (`sudo`) it refuses before touching
anything, and it refuses a relative `XDG_CONFIG_HOME`, `XDG_STATE_HOME`,
`HERDR_TRIGGERS_CONFIG_DIR`, `HERDR_TRIGGERS_STATE_DIR` or
`HERDR_APP_HOME`. Nothing is written until the whole plan is computed,
and the plan already checks that each file it writes can be written; a
step that still fails stops the run and names the step, and running
`install` again finishes the job. A `keys.json` that is not valid JSON
(normal while you edit it: the app keeps its last good keymap) skips the
two keys and nothing else; fix it and run `install` again.

| Item | What install does | What uninstall does |
|---|---|---|
| `mods/herdr-triggers/main.js`, `mod.json` under the app home (`HERDR_APP_HOME` or `~/.config/herdr-app`) | writes them from the binary; other files there are left alone | removes the two and the app's own `gpui-kit.d.ts` and `jsconfig.json` (plain files only), then the folder; anything else there keeps the folder, and the plan says what |
| `keys.json` `global`: `ctrl-alt-g`, `cmd-alt-g` | adds a missing key; a key bound (in any spelling of the same chord: `alt-ctrl-g`, `option-cmd-g`, any case) to something else, in `global` or in a `terminal` or `layout:<name>` section that takes precedence, is skipped, never overwritten; key order is kept, but the file is written back with 2-space indentation, and a key listed twice keeps only its last value. Never creates the file: the app writes it on first run, so open the app once before installing. After this edit the app stops adding its new default shortcuts to the file | removes only keys that still open this mod; the file stays |
| `~/.config/herdr-triggers` | links it to the TUI plugin's config when that exists and the path is free | leaves it: it points at your rules |
| login service | Linux: `herdr-triggers.service` in the user unit folder the systemd user manager searches (its `UnitPath`, read with `busctl --user`; `~/.config/systemd/user` when it lists none, and the plan says so), then `systemctl --user enable --now`. Where systemd does not run at all (no `/run/systemd/system`: a container, a distro without it) it is skipped, and `herdr-triggersd --host core start` runs the daemon detached. Where systemd runs but this session cannot reach the user manager (`su -`, `sudo -iu`, cron, a shell without `XDG_RUNTIME_DIR`), it is skipped too, with a note to run install from your desktop session, and a running daemon is left alone. macOS: `~/Library/LaunchAgents/io.github.cantona.herdr-triggers.plist`, then `launchctl bootstrap` | `disable --now` or `bootout`, then removes the file. Without systemd it only removes `~/.config/systemd/user/herdr-triggers.service`; with systemd out of reach from the session it leaves the service alone and says to run uninstall from your desktop session (the rest of uninstall still runs) |

The service runs the binary at the path your shell finds it (a
`~/.local/bin` symlink stays a symlink), else the one install runs from,
and the plan's service line names it (`runs <path>`): an install from a
build tree would tie the service to that tree. It runs with your shell's
`PATH`, so rule scripts and `notify-send` resolve as they do for you. Every
directory variable is pinned in the unit or plist as the command line
resolves it:
`XDG_CONFIG_HOME` and `XDG_STATE_HOME` always (defaults included), and
`HERDR_TRIGGERS_CONFIG_DIR`, `HERDR_TRIGGERS_STATE_DIR` and `HERDR_APP_HOME`
(for `tab_mark`) as your shell has them, or empty when it does not. So the
service reads the rules and state the command line does, whatever the
service manager's own environment says. Run `install` again after changing
any of them. A daemon for
the default core that the service did not start is stopped first, because
it would hold the lock; install checks the default core's daemon from
whichever core terminal it runs in. Should one hold the lock anyway, the
service logs `another herdr-triggersd holds the lock; waiting for it` once
and takes over when that daemon exits.

`herdr-triggersd uninstall` takes the same flags.

Rules and secrets live in `~/.config/herdr-triggers/` (`triggers.toml`, and
`secrets.toml` with mode 0600). The rule schema is in the README.

## What you see

**Dialog:** 560px wide, the theme's `popover` color, 18px padding, 8px
corners, 13px text.

- **Title row:** "Herdr Triggers" in 15px semibold `foreground`; the state
  word on the right: "running" (`working`), "waiting" (`idle`), "stopped"
  (`idle`), "error" (`blocked`); a dim "Esc" hint.
- **Daemon:** the process (`pid N`, `pid N, waiting for herdr-core`, or "not
  running"), the service ("systemd: installed", "launchd: installed", the
  same with ", watches the default core, not this one" in an app on another
  core session and " (not reachable from here)" when the app's session
  cannot reach the systemd user manager, or "not installed; run
  herdr-triggersd install"), the herdr-core socket,
  the config path with its rule count (or "config error" and the message),
  and the last fire: rule, terminal and local time.
- **Recent:** the last 20 daemon log lines in monospace, scrolling past
  220px. Fires, declines ("already on screen at startup", cooldowns) and
  failures all show here.
- **Buttons:** Start (primary) or Stop, Restart, Reload, Reset, and one
  sidebar button, whichever applies: "Clone layout for sidebar", "Switch to
  <name>-triggers" (the clone exists already), "Add to sidebar", "Remove
  from sidebar", or "No sidebar in this layout".

**Sidebar block:** 30px high, in the left sidebar above its footer. An 8px
dot in the same state colors, "Triggers", and the rule count in `dim`. Below
80px wide only the dot shows. Click it to open the dialog.

## How it behaves

- **The service runs the daemon.** It starts at login and runs
  `herdr-triggersd --host core service`. herdr-core has no stable unit to
  bind to (it runs as transient, hash-named jobs), so the daemon waits for
  core's socket instead: it tries every 2s, attaches once the core answers,
  and when the core goes away it logs `herdr-core gone; waiting for it` and
  waits again. While it waits, `status` says "waiting for herdr-core" and
  the dialog's state word is "waiting". That starts only once the engine
  gives up on the core, 30s after it stopped answering; until then `status`
  and the dialog still say "running". Closing and reopening the app does
  not touch it. If core's config moves the socket to another name, the
  service logs `herdr-core's socket moved to <path>; restarting to follow
  it` and exits 1, and the service manager starts it again with the state
  directory for the new name.
- **Start, Stop, Restart.** With the service installed, Start (and
  `herdr-triggersd --host core start`) asks the service manager to start
  it, so it stays supervised. That holds only for the default core the
  service watches; a start for another core session (`--session work`, or
  from one of its terminals) detaches a daemon for that core. The service
  is restarted 5s after any exit (`Restart=always`, launchd's `KeepAlive`),
  so a plain `kill` or a crash does not turn triggers off. Stop therefore
  goes through the service manager too (`systemctl --user stop`, or
  `launchctl bootout` of the agent), and holds until Start, Restart, a
  re-run of `install` or the next login. A daemon the service did not start
  is stopped as well, after the service, so nothing takes its place. If the
  manager refuses, `stop` reports its error and exits 1 without signalling
  the service's daemon, which the manager would only start again. From a
  session that cannot reach the systemd user manager at all (`su -`, cron),
  `start`, `stop` and `restart` aimed at the service's core signal and start
  nothing: they exit 1 and say to run them from your desktop session, and
  `status` shows the service as installed but not reachable (the dialog:
  "installed (not reachable from here)"). For another core, or without
  the service, Stop sends SIGTERM as before;
  there, a service waiting behind a stopped daemon takes over within 2s, and
  `stop` says so when it sees that happen. Programs a `run` action started
  keep running through a Stop or Restart of the service
  (`KillMode=process`, launchd's `AbandonProcessGroup`), as they did beside
  a detached daemon. Restart is Stop, then Start; when the manager refuses
  the stop but nothing is left running, Restart warns and starts anyway
  (falling back to a detached daemon as Start does). Without the service,
  Start detaches a daemon as in 0.2.0, and so does a Start whose service
  manager refuses (no user bus in that session, say): it prints the
  manager's error and says it is starting a detached daemon instead.
- **The block only shows state.** It reads status when it opens, and again
  a few times, 3s apart, while the daemon is still waiting for the core. It
  starts nothing.
- **Adding the block.** Shipped layouts refuse structural edits. On a
  shipped layout the button first clones it to `<name>-triggers` and the app
  switches to the clone; press "Add to sidebar" then. The block is inserted
  into the layout node `sidebar`, before `sidebar-footer`. For a layout
  without a `sidebar` column, place it by hand:

  ```json
  { "id": "herdr-triggers", "type": "block", "size": { "px": 30 },
    "content": { "mod": "herdr-triggers", "props": { "strip": true } } }
  ```

- **Rules fire as in the TUI.** `poll_ms` (default 200ms), `cooldown_ms`,
  `max_writes_per_second`, `tail_within` and `once` are the same code.
- **Cost.** The daemon learns about terminals from herdr-core's event stream;
  a poll reads only the screens a rule watches. Typing goes to the terminal
  as raw bytes through `terminal.attach`, exactly the rule's text.
- **The core leaving.** After herdr-core has been unreachable for 30s the
  engine lets go of it; the service then waits for it again. A daemon
  started with `run` or a detached `start` exits instead.
- **No standing timers in the mod.** Status is read when a copy starts,
  after each button, and in the block's short settle loop above. The block
  renders the state word and rule count the dialog shares; the log and
  messages stay in the dialog, because the app saves shared values to a file
  other local users can read.

## Differences from the TUI host

| Action | herdr TUI | Herdr app (herdr-core) |
|---|---|---|
| `notify` | herdr's toast, plus a desktop notification when `desktop` is set | a desktop notification only; the dialog's log shows the fire |
| `tab_mark` | `tab.rename` | `herdr-app do tab.rename`; needs the window open |
| `[settings] source` | `visible`, `recent`, ... | ignored: the daemon reads only the screen; `lines` keeps its last N lines |
| `pane_title` scope | pane title, tab label, workspace label | terminal title, space name, tab name |

## Command line

```sh
herdr-triggersd install|uninstall [--yes | --dry-run]
herdr-triggersd --host core status --json
herdr-triggersd --host core start|stop|restart|reload|reset
herdr-triggersd --host core service                 # what the login service runs
herdr-triggersd --host core status --session work   # another hc session
herdr-triggersd --host core restart --all           # every hc session
herdr-triggersd --host core status --json --all     # one JSON object per line
```

State lives in `~/.local/state/herdr-triggers/core-<session>/`.

## Built from

- `herdr` module: `exec`, `props`, `closeOverlay`, `theme`, `notify`, `run`,
  `layout`, `layouts`, `shared`, `setShared`.
- `gpui-kit` / `gpui-base`: `View`, `div`, `h_flex`, `v_flex`.
- App commands: `mod.open`, `layout.clone`, `layout.switch`, `layout.set`.
- `herdr-triggersd --host core status --json` for every fact. No network,
  no `hc`. No art.

## Watch out

- `herdr-triggersd` must be on the PATH the app process has: the dialog's
  `exec` runs it by bare name. (The service does not need this; its unit
  names the binary by full path.) `cargo install` puts it in `~/.cargo/bin`, which a Linux
  desktop session started outside your shell may not have; link it into a
  directory that session has (for example `~/.local/bin`) if the dialog
  says herdr-triggersd could not be started.
- The dialog acts on the herdr-core session that `HC_SESSION` names in the
  environment the app's `exec` provides (`default` without it).
- The service always watches the default core session, at the socket
  core's config `socket` key names if it has one; `HC_SESSION` and
  `HC_SOCKET` in the service manager's environment are ignored. Another
  session needs its own `herdr-triggersd --host core start --session <name>`, which detaches
  as before.
- `--follow-app` still works for a 0.2.0 mod or a script, but nothing in
  0.2.1 passes it.
- On herdr-core a `notify` that the desktop notifier could not show (it is
  missing or it failed) keeps the rule armed, so a `once` rule is not used
  up; it is retried with backoff, as a missing `run` program is.
- `tab_mark` takes the tab's name from herdr-core's `where` label. On a tab
  nobody named, that is its number, so marking it names it ("! 1"), and the
  name no longer follows the tab's place; rename it to "" to undo. A space
  name containing " / " confuses the split.
- A rule that types a credential belongs on a scoped, `tail_within` rule, as
  in the TUI. The app's terminals are watched only while the daemon runs.

## Done when

- `herdr-app check mods/herdr-triggers` passes.
- `ctrl-alt-g` (`cmd-alt-g` on macOS) opens the dialog; Esc closes it.
- `herdr-triggersd install --dry-run` lists the plan; `install --yes`
  applies it, and a second `install` reports every line `unchanged`.
- `systemctl --user status herdr-triggers` (Linux) or `launchctl print
  gui/$(id -u)/io.github.cantona.herdr-triggers` (macOS) shows the job
  running, and the dialog shows "Service: ... installed".
- Start makes the state word "running"; Stop makes it "stopped", and it
  stays stopped. `kill` of the service's pid brings it back within about
  5s.
- Closing and reopening the app leaves the daemon running, and a rule still
  fires.
- With `herdr-triggersd` off PATH, the dialog shows the install command.
