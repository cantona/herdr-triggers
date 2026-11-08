//! What the rule engine needs from a terminal server.
//!
//! The herdr TUI socket and the herdr-core socket answer the same questions
//! in different dialects. The engine and the actions speak only this trait,
//! so a rule behaves identically on both hosts and the fire path has no
//! host-specific branch in it.

use std::fmt;
use std::process::{Command, Stdio};

use crate::client::ClientError;

/// One watched terminal. `pane_id` is what reads and typing address;
/// `terminal_id` keys the once-ledger because it survives a pane being
/// recreated. On herdr-core both are the terminal address.
#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub pane_id: String,
    pub terminal_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    /// Every name a `pane_title` scope may match: the pane's own title, its
    /// tab's and workspace's labels, with and without a `[N] ` position prefix.
    pub titles: Vec<String>,
}

impl PaneInfo {
    pub fn ledger_id(&self) -> &str {
        if self.terminal_id.is_empty() {
            &self.pane_id
        } else {
            &self.terminal_id
        }
    }
}

pub struct Notification<'a> {
    pub title: &'a str,
    pub body: Option<&'a str>,
    pub sound: Option<&'a str>,
    pub position: Option<&'a str>,
    /// `desktop = "normal" | "critical"` on the rule: raise a desktop
    /// notification as well (herdr host) or as the notification (core host).
    pub desktop: Option<&'a str>,
}

#[derive(Debug)]
pub enum ServerError {
    Client(ClientError),
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServerError::Client(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ServerError {}

impl From<ClientError> for ServerError {
    fn from(err: ClientError) -> Self {
        ServerError::Client(err)
    }
}

pub trait Server: Send + Sync {
    /// The socket this server resolved to, for the startup log line.
    fn describe(&self) -> String;
    /// Every pane the daemon may watch. `with_labels` asks for tab and
    /// workspace names in `titles`, which may cost a host extra calls.
    fn panes(&self, with_labels: bool) -> Result<Vec<PaneInfo>, ServerError>;
    /// The pane's text as rules see it. `source` and `lines` are the
    /// `[settings]` values; a host ignores what it cannot honour.
    fn read(&self, pane_id: &str, source: &str, lines: Option<u32>) -> Result<String, ServerError>;
    /// Types `text` into the pane exactly as given, nothing appended.
    fn send_text(&self, pane_id: &str, text: &str) -> Result<(), ServerError>;
    /// The tab's current label with any `[N] ` decoration removed.
    fn tab_label(&self, tab_id: &str) -> Result<String, ServerError>;
    fn rename_tab(&self, tab_id: &str, label: &str) -> Result<(), ServerError>;
    /// Posts a notification and returns the host's reason word: `shown`,
    /// `disabled`, `busy`, or whatever else it reports.
    fn notify(&self, n: &Notification<'_>) -> Result<String, ServerError>;
    /// Whether this failure means the daemon should exit instead of
    /// retrying. The herdr host retries forever.
    fn is_fatal(&self, _err: &ServerError) -> bool {
        false
    }
}

/// A desktop notification via libnotify's CLI, which exists on Linux and BSD
/// desktops only. "critical" asks the desktop to keep it until dismissed;
/// whether it obeys is the notification server's choice.
///
/// The title and body become argv, and /proc/<pid>/cmdline is world-readable
/// for the life of that process: `${secret:}` cannot get here (refused at
/// load), but `${match}` carries whatever the pane printed.
pub fn raise_desktop_notification(
    urgency: &str,
    title: &str,
    body: Option<&str>,
) -> Result<(), String> {
    let mut command = desktop_notification_command(urgency, title, body);
    let program = command.get_program().to_string_lossy().into_owned();
    match command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            // Reap it, or a trigger that fires often leaves a zombie per fire.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(())
        }
        Err(err) => Err(format!("cannot run {program}: {err}")),
    }
}

fn desktop_notification_command(urgency: &str, title: &str, body: Option<&str>) -> Command {
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
    command
}

/// Strips herdr's `[N] ` tab-number decoration, so a scope can match the
/// name the user gave the tab regardless of where it sits, and a re-marked
/// label does not bake the number in.
pub fn strip_tab_number(label: &str) -> Option<&str> {
    let rest = label.strip_prefix('[')?;
    let (number, rest) = rest.split_once("] ")?;
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_number_decoration_is_recognised_only_in_its_exact_shape() {
        assert_eq!(strip_tab_number("[1] bash"), Some("bash"));
        assert_eq!(strip_tab_number("[12] OK bash"), Some("OK bash"));
        assert_eq!(strip_tab_number("bash"), None);
        assert_eq!(strip_tab_number("[x] bash"), None);
        assert_eq!(strip_tab_number("[1]bash"), None);
    }

    #[test]
    fn the_ledger_id_prefers_the_terminal_and_falls_back_to_the_pane() {
        let with = PaneInfo {
            pane_id: "w1:p1".into(),
            terminal_id: "term-9".into(),
            tab_id: String::new(),
            workspace_id: String::new(),
            titles: vec![],
        };
        assert_eq!(with.ledger_id(), "term-9");
        let without = PaneInfo {
            terminal_id: String::new(),
            ..with
        };
        assert_eq!(without.ledger_id(), "w1:p1");
    }
}
