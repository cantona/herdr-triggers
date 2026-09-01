//! `triggers.toml` and `secrets.toml` loading.
//!
//! The rule schema covers the actions that are portable to herdr's socket API. Secrets never live in the manifest
//! or in `triggers.toml`: rules reference them as `${secret:NAME}` and the value
//! is read from a 0600 `secrets.toml` only when a rule fires.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default fires-per-second ceiling for actions that write into a pane.
/// A write-burst cap exists to break feedback loops
/// where a rule's own output re-matches the rule.
pub const DEFAULT_MAX_WRITES_PER_SECOND: u32 = 8;
/// Default quiet period per (rule, pane, matched line) after a fire.
///
/// Only a floor against a storm; the latch is what stops a prompt being
/// answered twice. Keep it short - it is keyed on the matched text, so a
/// console reprompting with identical text is delayed by exactly this long.
pub const DEFAULT_COOLDOWN_MS: u64 = 100;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggersConfig {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default = "default_cooldown_ms")]
    pub cooldown_ms: u64,
    #[serde(default = "default_max_writes_per_second")]
    pub max_writes_per_second: u32,
    /// Screen region matched against. `visible` is the default and the only
    /// one that is cheap and side-effect free; see `default_source`.
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default)]
    pub lines: Option<u32>,
    /// Whether a rule may fire on text that was already on screen when it armed.
    ///
    /// Default false, and deliberately so: matching is against screen
    /// snapshots, so a rule armed while a finished login is still visible
    /// cannot tell that text from a fresh prompt, and firing would type the
    /// secret at a shell prompt.
    /// Turn it on to answer a prompt that is already waiting when the daemon
    /// starts.
    #[serde(default)]
    pub fire_on_existing_text: bool,
    /// How often each watched pane's screen is read, in milliseconds. This is
    /// the response time: a prompt is answered within one interval of
    /// appearing. Clamped to 50..=60000.
    ///
    /// Each interval costs three list calls (panes, tabs, workspaces) plus one
    /// `pane.read` per watched pane, each on its own short-lived connection, so
    /// a rule's `scope` decides how much work this is. Keep
    /// `source = "visible"`: it needs no scrollback and is the cheapest read.
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
    /// How much the daemon writes to its own log: `off`, `fires` or `all`.
    ///
    /// Defaults to `all`. This file is the only record of what a resident
    /// daemon did, so quietness is opt-in.
    ///
    /// `fires` drops exactly the two lines that explain a rule declining to
    /// act - a match outside `tail_within`, and one held by the cooldown - and
    /// keeps everything else, failures included. Both are deduped per
    /// occurrence, so this is about kind rather than volume. `off` writes
    /// nothing once the config has loaded. Note the log records matched prompt
    /// text, which on a console pane can be worth suppressing.
    #[serde(default = "default_log")]
    pub log: String,
}

fn default_log() -> String {
    "all".to_string()
}

fn default_poll_ms() -> u64 {
    200
}

fn default_cooldown_ms() -> u64 {
    DEFAULT_COOLDOWN_MS
}

fn default_max_writes_per_second() -> u32 {
    DEFAULT_MAX_WRITES_PER_SECOND
}

fn default_source() -> String {
    // `visible` on purpose. A socket client cannot ask for a passive read -
    // herdr skips the `intent` field, so every read is Interactive - and for
    // the `recent*` sources herdr answers an Interactive read on an idle
    // agent pane by injecting wheel events to harvest its scrollback. Polling
    // that several times a second would synthetically scroll other people's
    // TUIs. `visible` never takes that path.
    "visible".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            cooldown_ms: default_cooldown_ms(),
            max_writes_per_second: default_max_writes_per_second(),
            source: default_source(),
            lines: None,
            fire_on_existing_text: false,
            poll_ms: default_poll_ms(),
            log: default_log(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// `regex` crate syntax, so backreferences and lookaround are not
    /// available.
    pub regex: String,
    #[serde(default)]
    pub once: bool,
    #[serde(default)]
    pub scope: Option<Scope>,
    /// Fire only when the match sits within this many non-blank lines of the
    /// screen's tail (0 = it must be the last non-blank line).
    ///
    /// herdr exposes no cursor position, so a rule otherwise matches a prompt
    /// that scrolled up long after it was answered. For a rule that types a
    /// credential that is the difference between answering the live prompt and
    /// typing a password at whatever now sits below it. A waiting prompt is at
    /// the tail (allow 1 for a status line, as a full-screen client draws one);
    /// an answered one has output beneath it.
    #[serde(default)]
    pub tail_within: Option<usize>,
    pub action: Action,
}

/// Optional filter deciding which panes a rule arms on. Every field is a regex
/// matched against the corresponding pane attribute; all present fields must
/// match.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    #[serde(default)]
    pub pane_title: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub pane_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// Types text into the pane (`pane.send_text`).
    SendText { text: String },
    /// Notification (`notification.show`).
    ///
    /// `sound` is the only way to make an alert stand out: herdr's API carries
    /// no colour or text styling of any kind, so a rule cannot recolour the
    /// line it matched.
    Notify {
        title: String,
        #[serde(default)]
        body: Option<String>,
        /// `none` (default), `done`, or `request`.
        #[serde(default)]
        sound: Option<String>,
        /// Where the toast appears, if herdr's config offers a choice.
        #[serde(default)]
        position: Option<String>,
        /// Additionally raise a DESKTOP notification, at this urgency:
        /// "low", "normal" or "critical".
        ///
        /// Linux only: it shells out to libnotify's `notify-send`, which macOS
        /// has no equivalent of. Elsewhere the rule still fires and the herdr
        /// toast still shows; only this extra notification is skipped, with a
        /// line in the log.
        ///
        /// This exists because herdr's own toast cannot be made to persist: an
        /// API notification is always its shortest kind, a fixed 3 seconds, and
        /// `ui.toast.delay_seconds` does not apply to it. The desktop
        /// notification is raised IN ADDITION to the in-terminal toast, not
        /// instead of it.
        ///
        /// "critical" additionally sends `--expire-time=0`, which ASKS the
        /// desktop to keep it until dismissed - but that is the notification
        /// server's decision, and GNOME Shell ignores the hint for banner
        /// duration. Check your own desktop before relying on it; `tab_mark` is
        /// the only indicator here that genuinely persists.
        #[serde(default)]
        desktop: Option<String>,
    },
    /// Local subprocess, detached from the daemon.
    Run {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        /// Secrets to hand the child as environment variables of the same name.
        ///
        /// This exists because `${secret:NAME}` is refused in `args`: argv is
        /// readable by every user on the machine through /proc, while a
        /// process's environment is readable only by its owner.
        #[serde(default)]
        secret_env: Vec<String>,
    },
    /// Local subprocess fed the pane snapshot on stdin; its stdout is typed back.
    Coprocess {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        feed_screen: bool,
        /// See `Run::secret_env`.
        #[serde(default)]
        secret_env: Vec<String>,
    },
    /// herdr has no tab-colour API; the closest portable effect is marking the
    /// tab label. An approximation, documented as such in the README.
    TabMark { marker: String },
}

/// Configuration that is valid but likely to do damage, reported at load time
/// rather than discovered by a password landing in the wrong pane.
pub fn warnings(rules: &[Rule]) -> Vec<String> {
    let mut warnings = Vec::new();
    for (index, rule) in rules.iter().enumerate() {
        let unscoped = rule.scope.is_none();
        if unscoped && rule.action.writes_to_pane() {
            warnings.push(format!(
                "rule {index} ({}) writes into any pane whose output matches; add a scope so it \
                 cannot answer a prompt in the wrong pane",
                rule.regex
            ));
        }
        if let Action::Notify {
            desktop: Some(_),
            title,
            body,
            ..
        } = &rule.action
        {
            let expands_screen_text = |text: &str| {
                text.contains("${match}") || (1..=9).any(|n| text.contains(&format!("${n}")))
            };
            if expands_screen_text(title) || body.as_deref().is_some_and(expands_screen_text) {
                warnings.push(format!(
                    "rule {index} ({}) puts matched screen text in a desktop notification: that \
                     text becomes notify-send's argv, which every user on this machine can read \
                     from /proc while it runs. Fine for a log line; not for anything a matched \
                     line might carry.",
                    rule.regex
                ));
            }
        }
        if unscoped && action_uses_secret(&rule.action) {
            warnings.push(format!(
                "rule {index} ({}) sends a secret and has no scope",
                rule.regex
            ));
        }
    }
    warnings
}

/// A secret in one of these ends up displayed, and for a tab label, written to
/// disk by herdr. Refused at load rather than at fire time, so the mistake is
/// caught before a console is waiting on it.
pub fn secret_reaches_the_screen(action: &Action) -> bool {
    match action {
        Action::Notify { title, body, .. } => {
            title.contains("${secret:")
                || body
                    .as_deref()
                    .is_some_and(|body| body.contains("${secret:"))
        }
        Action::TabMark { marker } => marker.contains("${secret:"),
        _ => false,
    }
}

fn action_uses_secret(action: &Action) -> bool {
    match action {
        Action::SendText { text } => text.contains("${secret:"),
        Action::Notify { title, body, .. } => {
            title.contains("${secret:")
                || body
                    .as_deref()
                    .is_some_and(|body| body.contains("${secret:"))
        }
        Action::Run { secret_env, .. } | Action::Coprocess { secret_env, .. } => {
            !secret_env.is_empty()
        }
        Action::TabMark { .. } => false,
    }
}

impl Action {
    /// Stable across reloads and across daemon restarts: the identity is the rule's
    /// observable behaviour, not its position in the file.
    pub fn identity(&self) -> String {
        // serde_json with a struct-tagged enum is deterministic for these
        // shapes: field order follows the declaration, not a hash map.
        serde_json::to_string(self).unwrap_or_else(|_| format!("{self:?}"))
    }

    pub fn writes_to_pane(&self) -> bool {
        matches!(self, Action::SendText { .. } | Action::Coprocess { .. })
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Missing(PathBuf),
    Read(PathBuf, std::io::Error),
    Parse(PathBuf, String),
    InsecureMode(PathBuf, u32),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Missing(path) => write!(f, "{} does not exist", path.display()),
            ConfigError::Read(path, err) => write!(f, "cannot read {}: {err}", path.display()),
            ConfigError::Parse(path, err) => write!(f, "cannot parse {}: {err}", path.display()),
            ConfigError::InsecureMode(path, mode) => write!(
                f,
                "{} must be mode 0600, found {:04o}",
                path.display(),
                mode
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// The sources this daemon will read. `recent*` are refused separately just
/// above - herdr answers those with a scrollback harvest - so only these two
/// remain reachable. A name herdr does not know would make every read fail and
/// show up merely as a daemon that never fires, so it is rejected at load.
const VALID_SOURCES: [&str; 2] = ["visible", "detection"];

pub fn load_triggers(path: &Path) -> Result<TriggersConfig, ConfigError> {
    if !path.exists() {
        return Err(ConfigError::Missing(path.to_path_buf()));
    }
    let text =
        std::fs::read_to_string(path).map_err(|err| ConfigError::Read(path.to_path_buf(), err))?;
    let config: TriggersConfig = toml::from_str(&text)
        .map_err(|err| ConfigError::Parse(path.to_path_buf(), err.to_string()))?;
    if crate::log::Level::parse(&config.settings.log).is_none() {
        return Err(ConfigError::Parse(
            path.to_path_buf(),
            format!(
                "settings.log {:?} must be off, fires or all",
                config.settings.log
            ),
        ));
    }
    if config.settings.source.starts_with("recent") {
        return Err(ConfigError::Parse(
            path.to_path_buf(),
            format!(
                "settings.source {:?} is refused: herdr answers a scrollback read on an idle agent \
                 pane by injecting wheel events to harvest it, which polling would do repeatedly. \
                 Use \"visible\".",
                config.settings.source
            ),
        ));
    }
    for (index, rule) in config.rules.iter().enumerate() {
        if secret_reaches_the_screen(&rule.action) {
            return Err(ConfigError::Parse(
                path.to_path_buf(),
                format!(
                    "rule {index} ({}) expands a secret into a notification or a tab label. Both \
                     are shown on screen, and herdr persists a tab label to its session file.",
                    rule.regex
                ),
            ));
        }
    }
    for (index, rule) in config.rules.iter().enumerate() {
        if let Action::Notify {
            desktop: Some(urgency),
            ..
        } = &rule.action
        {
            if !["low", "normal", "critical"].contains(&urgency.as_str()) {
                return Err(ConfigError::Parse(
                    path.to_path_buf(),
                    format!(
                        "rule {index}: notify desktop {urgency:?} must be low, normal or critical"
                    ),
                ));
            }
        }
    }
    if !VALID_SOURCES.contains(&config.settings.source.as_str()) {
        return Err(ConfigError::Parse(
            path.to_path_buf(),
            format!(
                "settings.source {:?} is not one of {}",
                config.settings.source,
                VALID_SOURCES.join(", ")
            ),
        ));
    }
    Ok(config)
}

/// Secrets are read on demand, never cached in the daemon and never logged.
///
/// The file must be 0600: a world- or group-readable secrets file is refused
/// rather than silently used, so a permissions mistake fails loudly.
pub fn load_secrets(path: &Path) -> Result<BTreeMap<String, String>, ConfigError> {
    if !path.exists() {
        return Err(ConfigError::Missing(path.to_path_buf()));
    }
    let mode = file_mode(path).map_err(|err| ConfigError::Read(path.to_path_buf(), err))?;
    if mode & 0o077 != 0 {
        return Err(ConfigError::InsecureMode(path.to_path_buf(), mode & 0o7777));
    }
    let text =
        std::fs::read_to_string(path).map_err(|err| ConfigError::Read(path.to_path_buf(), err))?;
    // A toml error's Display quotes the offending source line, which for
    // secrets.toml is a credential. Report position only, never the text: this
    // string reaches the log and `triggers-status`.
    toml::from_str(&text).map_err(|err| ConfigError::Parse(path.to_path_buf(), redact_toml(&err)))
}

/// A parse error reduced to its location, with the source snippet stripped.
fn redact_toml(err: &toml::de::Error) -> String {
    match err.span() {
        Some(span) => format!("malformed TOML near byte {} (details withheld)", span.start),
        None => "malformed TOML (details withheld)".to_string(),
    }
}

#[cfg(unix)]
fn file_mode(path: &Path) -> std::io::Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    Ok(std::fs::metadata(path)?.permissions().mode())
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> std::io::Result<u32> {
    Ok(0o600)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, contents: &str, mode: u32) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-triggers-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        path
    }

    #[test]
    fn parses_the_login_shape_from_the_readme() {
        let path = temp_file(
            "triggers.toml",
            r#"
[[rules]]
regex = "login:"
once = true
scope = { pane_title = "console.*" }
action = { type = "send_text", text = "${secret:DEVICE_USER}\n" }

[[rules]]
regex = "Password:"
once = true
action = { type = "send_text", text = "${secret:DEVICE_PASS}\n" }
"#,
            0o644,
        );
        let config = load_triggers(&path).expect("config parses");
        assert_eq!(config.rules.len(), 2);
        assert!(config.rules[0].once);
        assert_eq!(
            config.rules[0]
                .scope
                .as_ref()
                .unwrap()
                .pane_title
                .as_deref(),
            Some("console.*")
        );
        assert_eq!(config.settings.max_writes_per_second, 8);
    }

    #[test]
    fn unknown_keys_are_rejected_rather_than_ignored() {
        let path = temp_file(
            "triggers.toml",
            "[[rules]]\nregex = \"x\"\ntypo = true\naction = { type = \"tab_mark\", marker = \"!\" }\n",
            0o644,
        );
        let err = load_triggers(&path).expect_err("unknown key must fail");
        assert!(matches!(err, ConfigError::Parse(_, _)), "got {err:?}");
    }

    #[test]
    fn an_unscoped_rule_that_types_a_secret_is_reported() {
        let path = temp_file(
            "triggers.toml",
            r#"
[[rules]]
regex = "login:"
action = { type = "send_text", text = "${secret:USER}\n" }

[[rules]]
regex = "Password:"
scope = { pane_title = "console.*" }
action = { type = "send_text", text = "${secret:PASS}\n" }
"#,
            0o644,
        );
        let config = load_triggers(&path).expect("config parses");
        let warnings = warnings(&config.rules);
        assert_eq!(warnings.len(), 2, "got {warnings:?}");
        assert!(warnings
            .iter()
            .all(|warning| warning.starts_with("rule 0 ")));
        assert!(warnings[1].contains("sends a secret"));
    }

    #[test]
    fn a_scoped_rule_and_a_tab_mark_are_not_reported() {
        let path = temp_file(
            "triggers.toml",
            r#"
[[rules]]
regex = "done"
action = { type = "tab_mark", marker = "! " }
"#,
            0o644,
        );
        let config = load_triggers(&path).expect("config parses");
        assert!(warnings(&config.rules).is_empty());
    }

    #[test]
    fn secrets_refuse_to_load_when_group_or_world_readable() {
        let path = temp_file("secrets.toml", "DEVICE_USER = \"admin\"\n", 0o640);
        let err = load_secrets(&path).expect_err("0640 must be refused");
        match err {
            ConfigError::InsecureMode(_, mode) => assert_eq!(mode, 0o640),
            other => panic!("expected InsecureMode, got {other:?}"),
        }
    }

    #[test]
    fn secrets_load_at_0600() {
        let path = temp_file("secrets.toml", "DEVICE_USER = \"admin\"\n", 0o600);
        let secrets = load_secrets(&path).expect("0600 loads");
        assert_eq!(
            secrets.get("DEVICE_USER").map(String::as_str),
            Some("admin")
        );
    }

    #[test]
    fn an_unknown_source_is_rejected_at_load() {
        let path = temp_file(
            "triggers.toml",
            "[settings]\nsource = \"recent-unwrapped\"\n",
            0o644,
        );
        let err = load_triggers(&path).expect_err("a bogus source must fail loudly");
        match err {
            ConfigError::Parse(_, message) => assert!(message.contains("settings.source")),
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[test]
    fn a_malformed_secrets_file_never_leaks_its_contents() {
        // A missing closing quote makes toml quote the offending line, which is
        // the secret. The reported error must carry position only.
        let path = temp_file(
            "secrets.toml",
            "DEVICE_PASS = \"hunter2-super-secret\n",
            0o600,
        );
        let err = load_secrets(&path).expect_err("malformed secrets must fail");
        let rendered = format!("{err}");
        assert!(
            !rendered.contains("hunter2"),
            "the parse error must not quote the secret: {rendered}"
        );
    }

    #[test]
    fn action_identity_is_stable_and_distinguishes_actions() {
        let a = Action::SendText {
            text: "hello".into(),
        };
        let b = Action::SendText {
            text: "hello".into(),
        };
        let c = Action::SendText {
            text: "other".into(),
        };
        assert_eq!(a.identity(), b.identity());
        assert_ne!(a.identity(), c.identity());
    }
}
