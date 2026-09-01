//! Running a fired rule's action.
//!
//! Action mapping:
//! `SendText` -> `pane.send_text`, `Notify` -> `notification.show`,
//! `Run`/`Coprocess` -> local subprocess, tab colouring -> `tab.rename` marker.
//! A renderer-level highlight has no socket equivalent and is not supported.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::client::{Client, ClientError};
use crate::config::{Action, ConfigError};
use crate::rules::CompiledRule;

/// A coprocess gets 10 seconds before giving up on it.
const COPROCESS_DEADLINE: Duration = Duration::from_secs(10);
/// The screen fed to a coprocess is capped at 48 KiB; the same cap bounds what
/// its output is allowed to type back into the pane.
const COPROCESS_FEED_MAX_BYTES: usize = 48 * 1024;
/// How finely the child is checked for exit, and for how long. A trigger script
/// typically answers in a few milliseconds, so a coarse check would add more
/// delay than the script itself takes.
const FAST_REAP_INTERVAL: Duration = Duration::from_millis(2);
const FAST_REAP_WINDOW: Duration = Duration::from_millis(250);
const SLOW_REAP_INTERVAL: Duration = Duration::from_millis(50);
/// After the child exits, how long to wait for its stdout pipe to fully close
/// before giving up on the drain (a lingering grandchild can hold it open).
const COPROCESS_DRAIN_GRACE: Duration = Duration::from_secs(2);

pub struct Context<'a> {
    pub pane_id: &'a str,
    pub tab_id: &'a str,
    pub matched_line: &'a str,
    pub screen: &'a str,
    pub secrets_path: &'a Path,
}

#[derive(Debug)]
pub enum ActionError {
    Client(ClientError),
    Secrets(ConfigError),
    UnknownSecret(String),
    SecretNotAllowed(String, &'static str),
    Spawn(String, std::io::Error),
}

impl ActionError {
    /// True when the failure is the user's to fix - a secret that cannot be
    /// resolved, or a program that is missing or not executable. Such a rule
    /// stays armed so the fix takes effect without a reload.
    pub fn is_users_to_fix(&self) -> bool {
        matches!(
            self,
            ActionError::Secrets(_)
                | ActionError::UnknownSecret(_)
                | ActionError::SecretNotAllowed(_, _)
                | ActionError::Spawn(_, _)
        )
    }
}

impl std::fmt::Display for ActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionError::Client(err) => write!(f, "{err}"),
            ActionError::Secrets(err) => write!(f, "secrets unavailable: {err}"),
            ActionError::UnknownSecret(name) => {
                write!(f, "secrets.toml has no entry named {name}")
            }
            ActionError::SecretNotAllowed(name, place) => write!(
                f,
                "${{secret:{name}}} cannot be expanded into {place}. For a subprocess, list \
                 {name} in secret_env so it arrives as an environment variable instead."
            ),
            ActionError::Spawn(program, err) => write!(f, "cannot run {program}: {err}"),
        }
    }
}

/// Expands `${secret:NAME}`, `${match}` and `$1`..`$9` in a rule's strings.
///
/// Secrets are read from disk per fire and dropped with the expanded string, so
/// the daemon never holds them longer than the action takes. Resolved values are
/// never returned in errors or written to the log.
/// Whether `${secret:NAME}` may be expanded into this particular string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Secrets {
    Allow,
    /// For anything that leaves the pane: argv (/proc/<pid>/cmdline is
    /// world-readable), a notification drawn on screen, or a tab label herdr
    /// persists in its session file.
    Refuse(&'static str),
}

pub fn expand(
    template: &str,
    rule: &CompiledRule,
    context: &Context<'_>,
) -> Result<String, ActionError> {
    expand_with(template, rule, context, Secrets::Allow)
}

pub fn expand_with(
    template: &str,
    rule: &CompiledRule,
    context: &Context<'_>,
    secrets: Secrets,
) -> Result<String, ActionError> {
    let mut result = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find('$') {
        result.push_str(&rest[..start]);
        let tail = &rest[start..];

        if let Some(body) = tail.strip_prefix("${") {
            let Some(end) = body.find('}') else {
                result.push_str(tail);
                return Ok(result);
            };
            let token = &body[..end];
            if let Some(name) = token.strip_prefix("secret:") {
                if let Secrets::Refuse(place) = secrets {
                    return Err(ActionError::SecretNotAllowed(name.to_string(), place));
                }
                result.push_str(&secret(name, context)?);
            } else if token == "match" {
                result.push_str(context.matched_line);
            } else {
                // Unknown placeholder: leave it verbatim rather than silently
                // expanding to nothing, which would hide a typo in a password.
                result.push_str(&tail[..end + 3]);
            }
            rest = &body[end + 1..];
            continue;
        }

        let digit = tail[1..].chars().next().filter(char::is_ascii_digit);
        match digit {
            Some(digit) => {
                let index = digit.to_digit(10).unwrap_or(0) as usize;
                let capture = rule
                    .regex
                    .captures(context.matched_line)
                    .and_then(|captures| captures.get(index).map(|m| m.as_str().to_string()))
                    .unwrap_or_default();
                result.push_str(&capture);
                rest = &tail[2..];
            }
            None => {
                result.push('$');
                rest = &tail[1..];
            }
        }
    }
    result.push_str(rest);
    Ok(result)
}

fn secret(name: &str, context: &Context<'_>) -> Result<String, ActionError> {
    let secrets =
        crate::config::load_secrets(context.secrets_path).map_err(ActionError::Secrets)?;
    secrets
        .get(name)
        .cloned()
        .ok_or_else(|| ActionError::UnknownSecret(name.to_string()))
}

/// Runs the action. The returned string is safe to log: it never contains an
/// expanded secret.
pub fn run(
    client: &Client,
    rule: &CompiledRule,
    context: &Context<'_>,
) -> Result<String, ActionError> {
    match &rule.action {
        Action::SendText { text } => {
            let expanded = expand(text, rule, context)?;
            client
                .request(
                    "pane.send_text",
                    json!({"pane_id": context.pane_id, "text": expanded}),
                )
                .map_err(ActionError::Client)?;
            // Deliberately not the byte count: for a credential rule that is
            // the password's length, and `triggers-status` prints the log.
            Ok("send_text".to_string())
        }
        Action::Notify {
            title,
            body,
            sound,
            position,
            desktop,
        } => {
            let refuse = Secrets::Refuse("a notification, which is drawn on screen");
            let title = expand_with(title, rule, context, refuse)?;
            let body = body
                .as_deref()
                .map(|body| expand_with(body, rule, context, refuse))
                .transpose()?;
            let mut params = json!({"title": title});
            if let Some(body) = body.as_deref() {
                params["body"] = json!(body);
            }
            if let Some(sound) = sound {
                params["sound"] = json!(sound);
            }
            if let Some(position) = position {
                params["position"] = json!(position);
            }
            let result = client
                .request("notification.show", params)
                .map_err(ActionError::Client)?;
            // Raised as well as the toast, not instead of it: this is the only
            // way to get a notification that outlives herdr's fixed 3 seconds.
            //
            // The expanded title and body become notify-send's argv, and
            // /proc/<pid>/cmdline is world-readable for the life of that
            // process. `${secret:}` cannot get here - it is refused at load -
            // but `${match}` and `$1`..`$9` carry whatever the pane printed,
            // so a rule matching a line that happens to contain a credential
            // would expose it. Hence the load-time warning on this pairing.
            if let Some(urgency) = desktop {
                raise_desktop_notification(urgency, &title, body.as_deref());
            }
            // herdr accepts the call and then decides whether to show it, so a
            // plain OK here means nothing. Reporting the reason is the
            // difference between "the rule works" and "the rule works and you
            // will never see it": with herdr's default delivery = off every
            // notification is discarded.
            let reason = result
                .get("reason")
                .and_then(|reason| reason.as_str())
                .unwrap_or("unknown");
            Ok(match reason {
                "shown" => "notify shown".to_string(),
                "disabled" => "notify DISCARDED: set [ui.toast] delivery = \"herdr\" in herdr's \
                               config; the default is off, which drops every notification"
                    .to_string(),
                other => format!("notify not shown ({other})"),
            })
        }
        Action::TabMark { marker } => {
            let marker = expand_with(
                marker,
                rule,
                context,
                Secrets::Refuse("a tab label, which herdr persists to disk"),
            )?;
            let label = marked_label(client, context.tab_id, &marker)?;
            client
                .request(
                    "tab.rename",
                    json!({"tab_id": context.tab_id, "label": label}),
                )
                .map_err(ActionError::Client)?;
            Ok(format!("tab_mark {label}"))
        }
        Action::Run {
            program,
            args,
            secret_env,
        } => {
            let args = expand_all(args, rule, context, Secrets::Refuse("a command argument"))?;
            let secrets = collect_secret_env(secret_env, context)?;
            spawn(program, &args, rule, context, secrets)?;
            Ok(format!("run {program}"))
        }
        Action::Coprocess {
            program,
            args,
            feed_screen,
            secret_env,
        } => {
            let args = expand_all(args, rule, context, Secrets::Refuse("a command argument"))?;
            let secrets = collect_secret_env(secret_env, context)?;
            let feed = feed_screen.then(|| {
                let screen = context.screen;
                let start = screen.len().saturating_sub(COPROCESS_FEED_MAX_BYTES);
                // Cut on a char boundary: the tail is what matters, and a split
                // codepoint would make the feed invalid UTF-8.
                let start = (start..screen.len())
                    .find(|index| screen.is_char_boundary(*index))
                    .unwrap_or(screen.len());
                screen[start..].to_string()
            });
            spawn_coprocess(client, program, &args, rule, context, secrets, feed)?;
            Ok(format!("coprocess {program}"))
        }
    }
}

/// A desktop notification via libnotify's CLI.
///
/// "critical" ASKS the desktop to keep it until dismissed; whether it obeys is
/// the notification server's choice. GNOME Shell, for one, ignores the expiry
/// hint for how long a banner stays up and files the notification in its shade
/// instead. A herdr toast cannot outlive 3 seconds at all, so this is still the
/// longer-lived of the two - but `tab_mark` is the only indicator that
/// genuinely persists, since it stays until the tab is renamed.
fn raise_desktop_notification(urgency: &str, title: &str, body: Option<&str>) {
    let mut command = Command::new("notify-send");
    command
        .arg("--app-name=herdr-triggers")
        .arg(format!("--urgency={urgency}"));
    if urgency == "critical" {
        // 0 is libnotify's "never expire". It is a request: a notification
        // server may cap or ignore it.
        command.arg("--expire-time=0");
    }
    command.arg("--").arg(title);
    if let Some(body) = body {
        command.arg(body);
    }
    match command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        // Reap it, or a trigger that fires often leaves a zombie per fire.
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        // notify-send is libnotify's CLI and exists on Linux/BSD desktops. On
        // macOS there is no equivalent, so `desktop` is a no-op there; the rest
        // of the plugin is portable.
        Err(err) => crate::log_line!("cannot run notify-send (Linux only): {err}"),
    }
}

fn expand_all(
    values: &[String],
    rule: &CompiledRule,
    context: &Context<'_>,
    secrets: Secrets,
) -> Result<Vec<String>, ActionError> {
    values
        .iter()
        .map(|value| expand_with(value, rule, context, secrets))
        .collect()
}

fn marked_label(client: &Client, tab_id: &str, marker: &str) -> Result<String, ActionError> {
    let tabs = client
        .request("tab.list", json!({}))
        .map_err(ActionError::Client)?;
    let current = tabs
        .get("tabs")
        .and_then(|tabs| tabs.as_array())
        .and_then(|tabs| {
            tabs.iter()
                .find(|tab| tab.get("tab_id").and_then(|id| id.as_str()) == Some(tab_id))
        })
        .and_then(|tab| tab.get("label"))
        .and_then(|label| label.as_str())
        .unwrap_or_default()
        .to_string();

    // herdr reports a label decorated with the tab number ("[1] bash"), and
    // writing that straight back would bake the number into the stored label,
    // one more copy per mark.
    let current = strip_tab_number(&current);

    // Re-marking a tab must not stack markers.
    if !marker.trim().is_empty() && current.contains(marker.trim()) {
        return Ok(current.to_string());
    }
    Ok(format!("{marker}{current}"))
}

fn strip_tab_number(label: &str) -> &str {
    let Some(rest) = label.strip_prefix('[') else {
        return label;
    };
    let Some((number, rest)) = rest.split_once("] ") else {
        return label;
    };
    if number.is_empty() || !number.chars().all(|character| character.is_ascii_digit()) {
        return label;
    }
    rest
}

fn collect_secret_env(
    names: &[String],
    context: &Context<'_>,
) -> Result<BTreeMap<String, String>, ActionError> {
    names
        .iter()
        .map(|name| Ok((name.clone(), secret(name, context)?)))
        .collect()
}

fn spawn(
    program: &str,
    args: &[String],
    rule: &CompiledRule,
    context: &Context<'_>,
    secrets: BTreeMap<String, String>,
) -> Result<(), ActionError> {
    let mut command = Command::new(program);
    command
        .args(args)
        // Only the owner can read /proc/<pid>/environ, unlike argv.
        .envs(secrets)
        .envs(action_env(rule, context))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = command
        .spawn()
        .map_err(|err| ActionError::Spawn(program.to_string(), err))?;

    // Fire and forget, but still reap: a trigger that runs on every prompt
    // would otherwise leave a zombie per fire.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// A coprocess is a conversation, not a launch: its stdout is typed back into
/// the pane, the way an interactive answer would be typed. A login flow
/// depends on this - the script reads the screen and answers the prompt.
fn spawn_coprocess(
    client: &Client,
    program: &str,
    args: &[String],
    rule: &CompiledRule,
    context: &Context<'_>,
    secrets: BTreeMap<String, String>,
    feed: Option<String>,
) -> Result<(), ActionError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(secrets)
        .envs(action_env(rule, context))
        .stdin(if feed.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = command
        .spawn()
        .map_err(|err| ActionError::Spawn(program.to_string(), err))?;

    // Feed on its own thread: a child that fills its stdout pipe before it
    // reads stdin would deadlock a blocking write_all here.
    if let Some(feed) = feed {
        if let Some(mut stdin) = child.stdin.take() {
            std::thread::spawn(move || {
                let _ = stdin.write_all(feed.as_bytes());
            });
        }
    }
    // Drain stdout on its own thread as well, so the deadline loop never
    // blocks on a pipe; the reader ends when the pipe closes (exit or kill).
    let stdout_rx = child.stdout.take().map(|mut pipe| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            let _ = tx.send(buffer);
        });
        rx
    });

    let client = client.clone();
    let pane_id = context.pane_id.to_string();
    let rule_id = rule.id.clone();
    std::thread::spawn(move || {
        let started = Instant::now();
        let deadline = started + COPROCESS_DEADLINE;
        let timed_out = loop {
            match child.try_wait() {
                Ok(Some(_)) => break false,
                Err(_) => break true,
                Ok(None) if Instant::now() >= deadline => {
                    // Killing also discards the output: typing a late answer at
                    // whatever runs in the pane ten seconds on would be worse
                    // than staying silent.
                    let _ = child.kill();
                    let _ = child.wait();
                    break true;
                }
                // These scripts answer in single-digit milliseconds, and this
                // wait is on the critical path of every prompt: check finely at
                // first, then back off so a long-running coprocess costs
                // nothing to wait on.
                Ok(None) => std::thread::sleep(if started.elapsed() < FAST_REAP_WINDOW {
                    FAST_REAP_INTERVAL
                } else {
                    SLOW_REAP_INTERVAL
                }),
            }
        };
        if timed_out {
            crate::log_line!("coprocess for rule {rule_id} timed out; output discarded");
            return;
        }
        let Some(rx) = stdout_rx else { return };
        // The child has exited, but read_to_end only returns once EVERY writer
        // closed the pipe - a grandchild that inherited stdout and lingers would
        // block this forever. Bound the drain so the thread cannot leak.
        let buffer = match rx.recv_timeout(COPROCESS_DRAIN_GRACE) {
            Ok(buffer) => buffer,
            Err(_) => {
                crate::log_line!(
                    "coprocess for rule {rule_id}: stdout still open after exit; output dropped"
                );
                return;
            }
        };
        if buffer.is_empty() {
            return;
        }
        let mut text = String::from_utf8_lossy(&buffer).into_owned();
        // Cap what gets typed back, as the fed screen is capped: a coprocess
        // emitting megabytes must not flood the pane. Truncate at a char
        // boundary at or below the cap.
        if text.len() > COPROCESS_FEED_MAX_BYTES {
            let mut end = COPROCESS_FEED_MAX_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            crate::log_line!(
                "coprocess for rule {rule_id}: output over {COPROCESS_FEED_MAX_BYTES} bytes, truncated"
            );
        }
        match client.request("pane.send_text", json!({"pane_id": pane_id, "text": text})) {
            Ok(_) => crate::log_line!("coprocess for rule {rule_id} answered the prompt"),
            Err(err) => {
                crate::log_line!("coprocess for rule {rule_id}: cannot type output back: {err}")
            }
        }
    });
    Ok(())
}

fn action_env(rule: &CompiledRule, context: &Context<'_>) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("HERDR_TRIGGER_RULE_ID".to_string(), rule.id.clone()),
        ("HERDR_TRIGGER_REGEX".to_string(), rule.regex_src.clone()),
        (
            "HERDR_TRIGGER_PANE_ID".to_string(),
            context.pane_id.to_string(),
        ),
        (
            "HERDR_TRIGGER_TAB_ID".to_string(),
            context.tab_id.to_string(),
        ),
        (
            "HERDR_TRIGGER_MATCHED_LINE".to_string(),
            context.matched_line.to_string(),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Rule;

    fn rule(regex: &str, action: Action) -> CompiledRule {
        CompiledRule::compile(&Rule {
            regex: regex.to_string(),
            once: false,
            scope: None,
            tail_within: None,
            action,
        })
        .expect("rule compiles")
    }

    fn secrets_file(contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-triggers-secret-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secrets.toml");
        std::fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    #[test]
    fn expands_secrets_captures_and_the_matched_line() {
        let secrets = secrets_file("DEVICE_USER = \"admin\"\n");
        let rule = rule(
            r"login as (\w+):",
            Action::SendText {
                text: String::new(),
            },
        );
        let context = Context {
            pane_id: "w1:p1",
            tab_id: "w1:t1",
            matched_line: "login as guest:",
            screen: "",
            secrets_path: &secrets,
        };

        assert_eq!(
            expand("${secret:DEVICE_USER}\n", &rule, &context).unwrap(),
            "admin\n"
        );
        assert_eq!(expand("$1", &rule, &context).unwrap(), "guest");
        assert_eq!(
            expand("${match}", &rule, &context).unwrap(),
            "login as guest:"
        );
        assert_eq!(expand("cost: $5.00", &rule, &context).unwrap(), "cost: .00");
    }

    #[test]
    fn an_unknown_secret_is_an_error_rather_than_an_empty_password() {
        let secrets = secrets_file("OTHER = \"x\"\n");
        let rule = rule(
            "login:",
            Action::SendText {
                text: String::new(),
            },
        );
        let context = Context {
            pane_id: "w1:p1",
            tab_id: "w1:t1",
            matched_line: "login:",
            screen: "",
            secrets_path: &secrets,
        };
        let err = expand("${secret:MISSING}", &rule, &context).expect_err("must fail");
        assert!(matches!(err, ActionError::UnknownSecret(name) if name == "MISSING"));
    }

    #[test]
    fn a_secret_is_refused_in_anything_that_becomes_argv() {
        let secrets = secrets_file("PASS = \"hunter2\"\n");
        let rule = rule(
            "x",
            Action::SendText {
                text: String::new(),
            },
        );
        let context = Context {
            pane_id: "w1:p1",
            tab_id: "w1:t1",
            matched_line: "x",
            screen: "",
            secrets_path: &secrets,
        };

        let err = expand_with(
            "--password=${secret:PASS}",
            &rule,
            &context,
            Secrets::Refuse("a command argument"),
        )
        .expect_err("argv must refuse secrets");
        let rendered = format!("{err}");
        assert!(matches!(err, ActionError::SecretNotAllowed(name, _) if name == "PASS"));
        assert!(
            !rendered.contains("hunter2"),
            "the refusal must not quote the secret it refused"
        );

        assert_eq!(
            expand_with("${secret:PASS}", &rule, &context, Secrets::Allow).unwrap(),
            "hunter2",
            "send_text still expands: it goes to the pane, not to argv"
        );
    }

    #[test]
    fn tab_number_decoration_is_not_baked_into_the_label() {
        assert_eq!(strip_tab_number("[1] bash"), "bash");
        assert_eq!(strip_tab_number("[12] OK bash"), "OK bash");
        assert_eq!(strip_tab_number("bash"), "bash");
        assert_eq!(strip_tab_number("[x] bash"), "[x] bash");
        assert_eq!(strip_tab_number("[1]bash"), "[1]bash");
    }

    #[test]
    fn unknown_placeholders_are_left_verbatim() {
        let secrets = secrets_file("A = \"1\"\n");
        let rule = rule(
            "x",
            Action::SendText {
                text: String::new(),
            },
        );
        let context = Context {
            pane_id: "w1:p1",
            tab_id: "w1:t1",
            matched_line: "x",
            screen: "",
            secrets_path: &secrets,
        };
        assert_eq!(
            expand("${nope}", &rule, &context).unwrap(),
            "${nope}",
            "a typo must be visible, not silently empty"
        );
    }
}
