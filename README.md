# herdr-triggers

Resident regex triggers over herdr pane output.

Rules live in a config file, stay armed for the life of the session, and fire an
action when their regex appears in a pane.

Any regex, any of the actions below, on any pane a scope selects — the two
worked examples further down are the cases this was built for, not the limit of
what it does. Anything a terminal prints can drive a notification, a tab mark,
a subprocess, or text typed straight back into the pane.

## Install

```bash
herdr plugin install cantona/herdr-triggers
```

herdr clones the repo to a temporary directory, shows you the manifest and the
commands it intends to run, and asks to confirm — cancelling discards the
clone. On confirmation it runs the `[[build]]` step (`cargo build --release`,
so a Rust toolchain is needed), moves the checkout into place and registers the
plugin. Browse the marketplace at
[herdr.dev/plugins](https://herdr.dev/plugins/).

The `[[startup]]` hook runs when a herdr **server** starts, not when a plugin
is installed, so after a mid-session install start the daemon once yourself —
from then on every server start does it for you:

```bash
herdr plugin action invoke triggers-start --plugin herdr-triggers
```

Pin a version or track a branch with `--ref`, and skip the prompt with `--yes`
(or `-y`) — which is required when stdin is not a terminal, so CI must pass it:

```bash
herdr plugin install cantona/herdr-triggers --ref v0.1.0 --yes
```

Then write your rules — nothing fires until you do:

```bash
$EDITOR "$(herdr plugin config-dir herdr-triggers)/triggers.toml"
herdr plugin action invoke triggers-reload --plugin herdr-triggers
```

To remove it: `herdr plugin uninstall herdr-triggers`.

### From a checkout, for development

```bash
git clone https://github.com/cantona/herdr-triggers
herdr plugin link ./herdr-triggers
cargo build --release           # link does NOT run [[build]] - build by hand
```

`plugin link` skips the manifest's `[[build]]`, so a linked checkout needs that
build once, and again after every source change followed by
`herdr-triggersd restart --all`.

### Platforms

Linux and macOS. Everything works on both except the `desktop` notification
option, which shells out to `notify-send` and is therefore Linux only.

### How the daemon runs

One daemon per herdr server, started from the manifest's `[[startup]]` hook when
that server starts. herdr `wait()`s on hook commands with only 32 slots, so
`start` forks, `setsid`s and returns immediately; the daemon keeps running on
its own and writes its log to
`$HERDR_PLUGIN_STATE_DIR/<server>/triggersd.log`, where `<server>` is `default`
or `session-<name>`. herdr's own plugin log is no help here: it records the
hook command, which exits immediately, and knows nothing of the detached daemon
that command leaves running — so the daemon keeps its own record. The server
name is part of the path because several herdr servers can run at once and
their pidfiles and locks must not collide; `triggers-status` prints the socket
and state dir it resolved to.

## Example: automated login

One of many possible uses. Answer a device's `login:` and `Password:` prompts
from a 0600 secrets file instead of typing them.

```toml
# triggers.toml, in `herdr plugin config-dir herdr-triggers`
[[rules]]
regex       = "login:"
once        = true
scope       = { pane_title = "console.*" }
tail_within = 2          # only the prompt still at the screen tail
action      = { type = "send_text", text = "${secret:DEVICE_USER}\n" }

[[rules]]
regex       = "Password:"
once        = true
scope       = { pane_title = "console.*" }
tail_within = 2
action      = { type = "send_text", text = "${secret:DEVICE_PASS}\n" }
```

Both guards matter on a rule that types a credential: `scope` keeps it to the
panes you mean, and `tail_within` keeps it to the prompt actually waiting for
input rather than one that has scrolled up.

```toml
# secrets.toml, same directory, mode 0600 or the daemon refuses to read it
DEVICE_USER = "admin"
DEVICE_PASS = "..."
```

For a device that issues a per-attempt challenge, use a `coprocess` instead: it
is fed the screen on stdin and whatever it prints is typed back, so a script can
read the challenge, decide, and decline by printing nothing.

## Example: failed-SSH alert

A different shape entirely: watch a pane tailing auth logs and raise an alert on
a failed authentication. No secrets involved, so nothing here can be typed into
a pane.

```toml
[[rules]]
regex  = "(?i)(failed password|invalid user|authentication failure).*from [0-9a-fA-F.:]+"
scope  = { pane_title = "(?i)monitor" }
action = { type = "notify", title = "Failed SSH auth", body = "${match}",
           sound = "request", desktop = "critical" }

# Marks the tab so it stands out until you rename it back.
[[rules]]
regex  = "(?i)(failed password|invalid user|authentication failure).*from [0-9a-fA-F.:]+"
scope  = { pane_title = "(?i)monitor" }
action = { type = "tab_mark", marker = "! " }
```

Four things make this work, each learned the hard way:

- **Require a `from <address>` clause.** It is what a real sshd failure line
  always has and prose about one does not, so the rule does not fire on a log
  message being discussed on screen rather than emitted.
- **`desktop = "critical"` is the only route to an alert that outlives 3
  seconds.** herdr's own toast is created as its shortest kind, a hardcoded 3
  seconds, and `ui.toast.delay_seconds` does not apply to an API notification.
  The desktop notification is raised *in addition*, and `critical` sends
  `--expire-time=0`, which asks the desktop to keep it until dismissed. Whether
  it does is the notification server's choice: GNOME Shell, for one, ignores
  the expiry hint for how long a banner stays on screen, keeping the
  notification in its shade instead. Check your own desktop before relying on
  it — the `tab_mark` action is the indicator that genuinely persists, since it
  stays until you rename the tab.
- **herdr discards notifications by default.** `ui.toast.delivery` defaults to
  `off`, which accepts the call and drops it. The four values are `off`,
  `herdr` (an in-terminal toast), `terminal` (an escape sequence) and `system`
  (the desktop's own notifier). The daemon log says
  `notify DISCARDED` when this is the problem, and `notify shown` when it is
  not — herdr also holds a single toast slot, so a second alert within a few
  seconds logs `busy`.
- **Scope it.** Not for safety, but for cost: unscoped, the rule watches every
  pane in the session and can trip the 32-pane ceiling. `pane_title` matches
  the pane's own title, its tab label *and* its workspace label, with any
  `[N] ` position prefix stripped — so naming a workspace `monitor` is enough,
  and reordering it changes nothing.

## Where a rule can be aimed

`scope.pane_title` is the name-based one and usually what you want. Prefer it
over `workspace_id`: ids are assigned per herdr **server**, and a machine can
run several at once (a named session plus the default), each with its own `w1`.
One daemon runs per server off the same config, so an id that means "the console
workspace" on one server can mean something entirely different on another —
which for a rule that types a password is not a risk worth taking. A name is
unambiguous. Anchor it (`"(?i)^com$"`) when the name is short.

`pane_id` is the third option and the most exact — `{ pane_id = "^w1:p1$" }`
pins a rule to one pane. It is also the most brittle, since a pane id changes
when the pane is recreated, so reach for it to test something rather than to
write a lasting rule.

## Plugin actions

| Action | Effect |
|---|---|
| `triggers-start` | start the daemon if it is not already running |
| `triggers-reload` | re-read `triggers.toml`; already-fired `once` rules stay fired |
| `triggers-reset` | re-arm every `once` rule |
| `triggers-restart` | stop the daemon and start a fresh one; use after rebuilding |
| `triggers-status` | daemon state, rule count, recent activity |
| `triggers-stop` | stop the daemon until the next herdr start |

Invoke from a keybinding, or directly — the action id comes first, the plugin
is a flag:

```bash
herdr plugin action invoke triggers-reset --plugin herdr-triggers
```

Each action is also a subcommand of the daemon binary, which is what the
manifest runs (`herdr-triggersd start`, `… restart`, `… reload`, `… reset`,
`… status`, `… stop`). `reset` and `reload` reach a running daemon by signal, so
either route works.

### Targeting a server

**A daemon runs per herdr server**, and with no target a command acts on the
server the calling shell belongs to — whichever pane you happen to be in. That
is easy to get wrong: run `stop` from a pane in your default session and the
named session's daemon is untouched, with nothing to say so. Be explicit when
it matters:

```bash
herdr-triggersd restart --all              # every server on this machine
herdr-triggersd reload  --session com      # one named session
herdr-triggersd status  --session default  # the default server
herdr-triggersd stop    --socket <path>    # by socket path
```

`--all` runs the command once per server and prefixes each with its name.
`status` always prints the socket and state dir it resolved to, so the target is
visible rather than assumed. An unknown session name fails loudly.

`reload` is the light option after editing rules — it re-reads them without
dropping the once-ledger. `restart` is for after rebuilding the binary; it waits
for the old daemon to release its lock before starting the new one, because
`start` refuses while the lock is held.

## Rule schema

| Key | Meaning |
|---|---|
| `regex` | `regex` crate syntax, compiled and matched by the daemon itself |
| `once` | fire once per pane, until `triggers-reset` |
| `scope` | optional `pane_title` / `workspace_id` / `pane_id` regex filters |
| `tail_within` | fire only when the match is within N non-blank lines of the screen tail — use it on any rule that types a credential |
| `action` | one of the actions below |

`[settings]` accepts `cooldown_ms` (default 100), `max_writes_per_second`
(default 8), `source` (default `visible`), `lines` (how many lines of that
source to read, default all),
`fire_on_existing_text` (default false), `poll_ms` (default 200) and `log`
(default `all`) — `fire_on_existing_text` and `poll_ms` decide whether a prompt
gets answered at all, and how fast; both are explained below.

`log` controls the daemon's own file at
`$HERDR_PLUGIN_STATE_DIR/<server>/triggersd.log` — `<server>` being `default`
or `session-<name>` — (mode 0600, a 256 KiB ring):

| `log` | Records |
|---|---|
| `all` (default) | everything, including why a rule declined to act on a match |
| `fires` | what happened — fires, failures, config loads, stop |
| `off` | nothing, once the config has loaded (a config that fails to parse is still reported — otherwise the failure would be invisible) |

`all` is the default because this file is the **only** record of what a resident
daemon did: herdr's own plugin log covers commands that finished, which this
never does.

`fires` drops exactly two lines, both of which explain why a rule chose *not*
to act on something it matched:

- `rule … not fired: match is N lines above the tail, outside tail_within`
- `rule … held by cooldown_ms`

Both are deduped per occurrence, so they do not repeat every poll — dropping
them is about kind, not volume. Reach for `fires` when you want a record of
what the daemon *did* without the running commentary on what it declined.

Everything else is kept, failures included: a pane that cannot be read, an
action that errored, and the one-off `already on screen at startup` note, which
is the only explanation for a waiting prompt going unanswered.

Reach for `off` knowing the log also records matched prompt text, which on a
console pane may be reason enough.

A rule that writes into a pane with no `scope` is reported in the daemon log at
load time: an unscoped `send_text` answers a matching prompt in *any* pane,
which for a credential is the mistake worth catching early.

### Actions

| Action | Maps to |
|---|---|
| `send_text` | `pane.send_text` |
| `notify` | `notification.show`, plus optional `sound`, `position`, and `desktop` for a desktop notification that can persist (`desktop` is Linux only — it uses `notify-send`) |
| `run` | local subprocess |
| `coprocess` | local subprocess fed the pane snapshot on stdin; its stdout is typed back into the pane, 10 s deadline (a timeout discards the output) |
| `tab_mark` | `tab.rename` with a marker prefix — stands in for tab colouring, which herdr has no API for |

A renderer-level inline highlight is not available: herdr exposes no such
socket call, so there is no `highlight` action.

Strings in an action expand `${secret:NAME}`, `${match}` (the matched line) and
`$1`..`$9` (capture groups). Subprocesses also receive `HERDR_TRIGGER_RULE_ID`,
`HERDR_TRIGGER_PANE_ID`, `HERDR_TRIGGER_TAB_ID` and
`HERDR_TRIGGER_MATCHED_LINE`.

`${secret:NAME}` is **refused** in `run` and `coprocess` arguments, and the rule
fails with an error naming the offending secret. Argv is world-readable through
`/proc/<pid>/cmdline`, so a password there is visible to every user on the
machine. Pass secrets to a subprocess with `secret_env` instead — the named
entries arrive as environment variables of the same name, and a process's
environment is readable only by its owner:

```toml
[[rules]]
regex  = "deploy ready"
action = { type = "run", program = "./notify.sh", secret_env = ["WEBHOOK_TOKEN"] }
```

`HERDR_TRIGGER_MATCHED_LINE` carries whatever the pane printed. If a terminal
echoes a password back, that line reaches the subprocess environment too.

## Secrets

Secrets are never written in the manifest or in `triggers.toml`. `secrets.toml`
must be mode 0600 — a group- or world-readable file is refused rather than used,
so a permissions slip fails loudly. Values are read at fire time, dropped with
the expanded string, and never written to the log. A `${secret:NAME}` with no
entry is an error, not an empty string, so a typo cannot send an empty password.

## How matching actually works, and what it costs

The daemon reads each scoped pane's screen every `poll_ms` and runs the rules
itself. It deliberately does **not** use herdr's `pane.output_matched`
subscription: that reports a match only on a rising edge of "text present
anywhere on screen", which never falls while a console sits at its prompt, and
the only way to reset it is to tear down and rebuild the connection — measured
at 1–72 s to answer a prompt, depending on churn. Matching here costs three
list calls plus one `pane.read` per watched pane per interval, and answers
within one interval.

Matching is still against a **snapshot of the screen**, not a stream of lines,
and that shapes the limits below.

- **Fast output can be missed.** Text that appears and scrolls away inside one
  `poll_ms` may never be sampled. Prompts block waiting for input, so they are
  always seen; a fast-scrolling build log is a poor fit.
- **There is no absolute line identity.** Nothing distinguishes "this same
  prompt is still sitting there" from "the same text was printed again", so
  identical text within `cooldown_ms` is treated as one occurrence. Unrelated
  output on a busy pane can also make a still-visible line look new.
- **Every rule matches the current screen, wherever the text sits.** herdr's
  API exposes no cursor position, so there is no way to say "only the line the
  cursor is on". A prompt that has scrolled up but is still in the frame keeps
  matching long after it was answered — and for a credential rule that means
  typing a password at whatever now sits below it, such as a live shell.
  **`tail_within` is the answer:** only a match within N non-blank lines of the
  tail is the prompt actually waiting for input. A waiting prompt sits 0–1 lines
  from the tail (1 when a full-screen client draws a status line); an answered
  one has output beneath it. Put `tail_within = 2` on every rule that types a
  secret. Also set `source = "visible"` for a full-screen client, so scrollback
  from earlier attempts cannot match either.
- **`poll_ms` is the response time.** A prompt is answered within one interval
  of appearing. Measured on a two-stage login: `poll_ms = 100` gives ~190 ms at
  0.1 % CPU; 50 ms buys nothing, as the floor is herdr's own read pipeline.
  Cost per interval is three list calls (`pane.list`, `tab.list`,
  `workspace.list`) plus one `pane.read` per watched pane — and each request
  opens its own short-lived socket connection, so at the 32-pane ceiling and
  `poll_ms = 100` that is a few hundred connections a second. `scope` is what
  keeps it small: a couple of watched panes costs a fraction of that. At most 32
  panes are watched, and going over is logged once. (Unrelated to herdr's own
  limit of 32 concurrent plugin hook commands.)
  **`source` must stay `visible`**, and a `recent*` source is refused at load:
  a socket client cannot ask for a passive read, and herdr answers a scrollback
  read on an idle agent pane by injecting wheel events to harvest it — which
  polling would do several times a second to someone else's TUI.
- **The firing edge is the text arriving AT THE TAIL, and this daemon owns it.**
  A rule fires when its match appears within `tail_within` of the tail, and does
  not fire again while that same prompt sits there. Answering it pushes the
  match away from the tail, the latch falls, and the next prompt is a fresh
  edge. This is the practical equivalent of cursor-line-only firing, built from
  the tail position because herdr exposes no cursor.

  **The prompt is identified by its text alone**, deliberately not by anything
  else on screen. A snapshot cannot separate "a new prompt with the same text"
  from "the same prompt still waiting", so this errs towards not acting: a
  clock, a scrolling log or a refreshed block *above* an unanswered prompt must
  never mark it new, because the action would run again and re-submit a
  password. The cost is the opposite case — a console that clears and redraws a
  byte-identical prompt without any poll seeing the tail move on is not
  answered a second time, and says so in the log.
- **`once` is per terminal, not per rule.** A global key would let a login rule
  work in exactly one pane ever, so the ledger keys on the pane's **terminal
  id** — the never-reused instance id, not the public `w1:p2` number that herdr
  counts from 1 again each launch. The ledger persists across daemon restarts,
  and a terminal's entries are pruned once that terminal leaves the pane list.
  Because a herdr restart gives every pane a fresh terminal id, all `once` rules
  re-arm after herdr itself restarts — what you want for a new session, and
  worth knowing for a credential rule.
- **Rules do not fire on text that was already on screen when they armed.**
  This is `fire_on_existing_text = false`, the default: a rule armed while a
  finished login is still visible cannot tell that text from a fresh prompt, and
  firing would type the secret at whatever prompt is showing.
  **A console that waits indefinitely at a prompt needs the opposite.** Its
  prompt is already on screen whenever the rules arm, and the screen never
  changes again, so the default waits forever and nothing happens. Set
  `fire_on_existing_text = true` for those, and keep the action safe to repeat —
  a script that reads the screen and declines when the state is wrong.
- **Write bursts are capped** at `max_writes_per_second` per rule, so a rule
  whose own output re-matches it cannot spin.

## Known limitations of this build

- **`scope.pane_title` matches any name the pane carries.** A manual
  `herdr pane rename` label, an agent-set title, and the terminal's own title
  are all tested, and the scope matches if any of them does. A full-screen
  client often sets no title at all, so scope those by `workspace_id` or
  `pane_id`.
- **The watched set is recomputed every `poll_ms` from `pane.list`.** Panes
  appearing, closing or being renamed are picked up on the next interval, so
  there is no separate rebuild clock and nothing to go stale.
- **An action that fails is retried on a backoff, not on every poll.** A
  failure the user has to fix (a `secrets.toml` mode, a typo'd secret name)
  leaves the rule armed so the fix takes effect — but the console is sitting at
  the same prompt, so retries back off from 2 s to a 60 s ceiling instead of
  running the action ten times a second.
- **`$` followed by a digit is always a capture reference.** There is no escape,
  so a literal `$5` in an action string expands to capture group 5 (empty when
  there is none). Put such text outside the action, or in `secret_env`.
- **The daemon outlives herdr and reconnects.** After `herdr server stop` it
  waits and reattaches when herdr returns; after `plugin disable`/`unlink` it
  keeps running until `triggers-stop` (or the machine reboots). A single
  `flock` guarantees only one daemon regardless of how many `start`s race.

## Testing

```bash
cargo test                      # rule identity, ledger, dedup, secrets, expansion
./scripts/integration-test.sh   # live: fake login pane against a running herdr
```

The integration test splits a pane running a fake login script and checks five
things: that the daemon answers both prompts, that `tab_mark` renames the tab,
that `once` holds on a second login, that the ledger survives a daemon restart,
and that `triggers-reset` re-arms. Everything it writes lives in a temp dir, and
it asks the binary where that is rather than assuming; the installed plugin's
own config and ledger are untouched.
