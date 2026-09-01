//! Rule identity, the once-ledger, and the guards that stop a rule firing on
//! its own output.
//!
//! `fired_once` is keyed by a hash of the rule's behaviour, so editing an
//! unrelated rule or reordering the file does not re-arm a rule that already
//! fired. Writes are capped per second so a rule whose own action produces
//! matching output cannot spin.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regex::Regex;

use crate::config::{Action, Rule, Scope};

/// Stable rule identity: FNV-1a over the regex and the action's canonical form.
///
/// `DefaultHasher` is explicitly not used - its output is not guaranteed stable
/// across Rust releases, and this value is persisted in the once-ledger.
pub fn rule_id(regex: &str, action: &Action) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    eat(regex.as_bytes());
    eat(&[0x1f]);
    eat(action.identity().as_bytes());
    format!("{hash:016x}")
}

#[derive(Debug)]
pub struct CompiledScope {
    pub pane_title: Option<Regex>,
    pub workspace_id: Option<Regex>,
    pub pane_id: Option<Regex>,
}

impl CompiledScope {
    fn compile(scope: &Scope) -> Result<Self, regex::Error> {
        Ok(Self {
            pane_title: scope.pane_title.as_deref().map(Regex::new).transpose()?,
            workspace_id: scope.workspace_id.as_deref().map(Regex::new).transpose()?,
            pane_id: scope.pane_id.as_deref().map(Regex::new).transpose()?,
        })
    }

    pub fn matches(&self, pane_id: &str, workspace_id: &str, titles: &[String]) -> bool {
        let check = |pattern: &Option<Regex>, value: &str| {
            pattern.as_ref().is_none_or(|regex| regex.is_match(value))
        };
        let title_ok = self
            .pane_title
            .as_ref()
            .is_none_or(|regex| titles.iter().any(|title| regex.is_match(title)));
        check(&self.pane_id, pane_id) && check(&self.workspace_id, workspace_id) && title_ok
    }
}

/// A pane's screen with its blank lines dropped, built once per poll and shared
/// by every rule on that pane.
#[derive(Debug)]
pub struct Screen<'a> {
    lines: Vec<&'a str>,
}

impl<'a> Screen<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            lines: text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect(),
        }
    }
}

/// Where a rule matched: the line itself, and how far it sits from the tail.
///
/// Nothing here distinguishes two occurrences carrying the SAME text - see
/// `signature`, which is deliberately the line's text alone.
#[derive(Debug, Clone, Copy)]
pub struct Match<'a> {
    pub line: &'a str,
    /// Non-blank lines below it; 0 means it is the last thing on screen.
    pub below: usize,
}

impl Match<'_> {
    /// Identity of the prompt this daemon answered: its own text, and nothing
    /// else on the screen.
    ///
    /// Screen position is deliberately NOT part of this. A snapshot cannot tell
    /// a genuinely new prompt from the same one still waiting when both carry
    /// identical text, so any signal that reacts to the surrounding screen
    /// makes the choice for you in the dangerous direction: a clock, a
    /// scrolling log or a refreshed block ABOVE an unanswered prompt would mark
    /// it "new" and the action would run again - re-submitting a password.
    ///
    /// So this errs the other way. A rule re-fires only once the tail has
    /// actually moved on: the prompt is gone, or its text differs. Missing an
    /// occurrence is a no-op that gets logged; re-sending a credential is not.
    pub fn signature(&self) -> u64 {
        content_hash(self.line)
    }
}

#[derive(Debug)]
pub struct CompiledRule {
    pub id: String,
    pub regex_src: String,
    /// Used for the matching itself and for `$1`-style captures in the action.
    pub regex: Regex,
    pub once: bool,
    pub scope: Option<CompiledScope>,
    /// See `config::Rule::tail_within`.
    pub tail_within: Option<usize>,
    pub action: Action,
}

impl CompiledRule {
    pub fn compile(rule: &Rule) -> Result<Self, regex::Error> {
        Ok(Self {
            id: rule_id(&rule.regex, &rule.action),
            regex_src: rule.regex.clone(),
            regex: Regex::new(&rule.regex)?,
            once: rule.once,
            scope: rule
                .scope
                .as_ref()
                .map(CompiledScope::compile)
                .transpose()?,
            tail_within: rule.tail_within,
            action: rule.action.clone(),
        })
    }
    /// The rule's LAST matching line and how many non-blank lines sit below it.
    /// `None` when it matches nothing.
    ///
    /// The last match, not the first: with a prompt repeated on screen the live
    /// one is the most recent.
    ///
    /// `below` drives the tail test; `signature` identifies the prompt.
    pub fn last_match<'a>(&self, screen: &Screen<'a>) -> Option<Match<'a>> {
        let lines = &screen.lines;
        let index = lines.iter().rposition(|line| self.regex.is_match(line))?;
        Some(Match {
            line: lines[index],
            below: lines.len() - 1 - index,
        })
    }

    /// The match that counts as live: the last one, and close enough to the
    /// tail to be the prompt actually waiting for input. A rule without
    /// `tail_within` accepts a match anywhere on screen.
    ///
    /// With `tail_within` set and no screen to judge, the answer is `None` -
    /// for a credential rule that is the safe direction.
    pub fn live_match<'a>(&self, screen: &Screen<'a>) -> Option<Match<'a>> {
        self.last_match(screen)
            .filter(|found| self.tail_within.is_none_or(|limit| found.below <= limit))
    }

    pub fn applies_to(&self, pane_id: &str, workspace_id: &str, titles: &[String]) -> bool {
        self.scope
            .as_ref()
            .is_none_or(|scope| scope.matches(pane_id, workspace_id, titles))
    }
}

/// Which `once` rules have already fired, keyed by rule identity **and terminal
/// id**.
///
/// A single-window terminal could key this by rule alone. herdr is
/// multi-pane, and a global key would let a login rule work in exactly one pane
/// ever, so the key carries the pane's terminal id - the never-reused instance
/// id, not the public pane number, so a pane taking a recycled number after a
/// herdr restart does not inherit an old terminal's fired state. Entries are
/// pruned by `retain_panes` against the live pane list.
#[derive(Debug, Default)]
pub struct Ledger {
    path: Option<PathBuf>,
    fired: BTreeSet<String>,
}

fn ledger_key(rule_id: &str, terminal_id: &str) -> String {
    format!("{rule_id}\t{terminal_id}")
}

impl Ledger {
    pub fn load(path: &Path) -> Self {
        crate::log::tighten(path);
        let fired = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
            .map(BTreeSet::from_iter)
            .unwrap_or_default();
        Self {
            path: Some(path.to_path_buf()),
            fired,
        }
    }

    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self::default()
    }

    pub fn has_fired(&self, rule_id: &str, pane_id: &str) -> bool {
        self.fired.contains(&ledger_key(rule_id, pane_id))
    }

    pub fn mark_fired(&mut self, rule_id: &str, pane_id: &str) {
        self.fired.insert(ledger_key(rule_id, pane_id));
        self.persist();
    }

    /// re-arm every `once` rule.
    pub fn reset(&mut self) {
        self.fired.clear();
        self.persist();
    }

    /// Drops entries for panes (keyed by terminal id) that no longer exist.
    /// herdr's close/exit events are not reliable for every close, so this runs
    /// against the live pane list at every rebuild.
    pub fn retain_panes(&mut self, live: &std::collections::HashSet<&str>) {
        let before = self.fired.len();
        self.fired.retain(|key| {
            key.split('\t')
                .nth(1)
                .is_some_and(|pane| live.contains(pane))
        });
        if self.fired.len() != before {
            self.persist();
        }
    }

    pub fn len(&self) -> usize {
        self.fired.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.fired.is_empty()
    }

    fn persist(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let entries: Vec<&String> = self.fired.iter().collect();
        let Ok(text) = serde_json::to_string(&entries) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Write-then-rename: a daemon killed mid-write must not leave a
        // truncated ledger that silently re-arms every once rule. Created
        // owner-only - it records which prompts were answered in which pane.
        let temp = path.with_extension("tmp");
        let mut options = std::fs::OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let written = options.open(&temp).and_then(|mut file| {
            use std::io::Write;
            file.write_all(text.as_bytes())
        });
        if written.is_ok() {
            let _ = std::fs::rename(&temp, path);
        }
    }
}

/// A floor on how often one rule may act in one pane.
///
/// The latch is what stops a prompt being answered twice; this only bounds a
/// storm if a screen churns. Identical text within the cooldown is held;
/// different text is allowed straight through, so a login prompt followed by a
/// password prompt is never delayed.
#[derive(Debug)]
pub struct FireGuard {
    cooldown: Duration,
    max_writes_per_second: u32,
    last_fire: HashMap<String, (Instant, u64)>,
    writes: HashMap<String, VecDeque<Instant>>,
}

impl FireGuard {
    pub fn new(cooldown_ms: u64, max_writes_per_second: u32) -> Self {
        Self {
            cooldown: Duration::from_millis(cooldown_ms),
            max_writes_per_second,
            last_fire: HashMap::new(),
            writes: HashMap::new(),
        }
    }

    /// Whether a fire is permitted, WITHOUT spending anything. Pair it with
    /// `record` once the action has actually run, so a fire that never happened
    /// does not spend the retry's budget.
    pub fn would_allow(&self, rule_id: &str, pane_id: &str, matched_line: &str) -> bool {
        self.would_allow_at(rule_id, pane_id, matched_line, Instant::now())
    }

    fn would_allow_at(&self, rule_id: &str, pane_id: &str, line: &str, now: Instant) -> bool {
        let key = ledger_key(rule_id, pane_id);
        let content = content_hash(line);
        !self.last_fire.get(&key).is_some_and(|(when, last)| {
            *last == content && now.duration_since(*when) < self.cooldown
        })
    }

    /// Records that a fire happened, starting its cooldown.
    pub fn record(&mut self, rule_id: &str, pane_id: &str, matched_line: &str) {
        self.record_at(rule_id, pane_id, matched_line, Instant::now());
    }

    fn record_at(&mut self, rule_id: &str, pane_id: &str, line: &str, now: Instant) {
        self.last_fire
            .insert(ledger_key(rule_id, pane_id), (now, content_hash(line)));
    }

    /// Whether another write is within the per-second cap, without spending a
    /// slot; pair with `record_write`.
    pub fn would_allow_write(&self, rule_id: &str) -> bool {
        self.would_allow_write_at(rule_id, Instant::now())
    }

    fn would_allow_write_at(&self, rule_id: &str, now: Instant) -> bool {
        let live = self.writes.get(rule_id).map_or(0, |window| {
            window
                .iter()
                .filter(|when| now.duration_since(**when) < Duration::from_secs(1))
                .count()
        });
        (live as u32) < self.max_writes_per_second
    }

    pub fn record_write(&mut self, rule_id: &str) {
        self.record_write_at(rule_id, Instant::now());
    }

    fn record_write_at(&mut self, rule_id: &str, now: Instant) {
        let window = self.writes.entry(rule_id.to_string()).or_default();
        while window
            .front()
            .is_some_and(|when| now.duration_since(*when) >= Duration::from_secs(1))
        {
            window.pop_front();
        }
        window.push_back(now);
    }

    pub fn retain_panes(&mut self, live: &std::collections::HashSet<&str>) {
        self.last_fire.retain(|key, _| {
            key.split('\t')
                .nth(1)
                .is_some_and(|pane| live.contains(pane))
        });
    }
}

pub fn content_hash(line: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in line.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Action;

    fn compiled(regex: &str, tail_within: Option<usize>) -> CompiledRule {
        CompiledRule::compile(&Rule {
            regex: regex.to_string(),
            once: false,
            scope: None,
            tail_within,
            action: Action::SendText {
                text: "x".to_string(),
            },
        })
        .expect("rule compiles")
    }

    /// A console waiting for input: the prompt is the last thing printed, with
    /// only the client's status line under it.
    const WAITING: &str = "\
Device model banner line
login: someuser
Password:
status line | 115200 8N1 | Offline";

    /// The same prompt after it was answered: a shell is running beneath it.
    const ANSWERED: &str = "\
Password:
device shell banner
Enter 'help' for a list of built-in commands.
~ #
~ #
status line | 115200 8N1 | Offline";

    #[test]
    fn tail_within_answers_the_live_prompt_and_ignores_the_scrolled_one() {
        let rule = compiled("^Password:", Some(2));
        assert_eq!(
            rule.last_match(&Screen::new(WAITING)).map(|m| m.below),
            Some(1),
            "status line sits below"
        );
        assert!(
            rule.live_match(&Screen::new(WAITING)).is_some(),
            "the waiting prompt must fire"
        );

        assert_eq!(
            rule.last_match(&Screen::new(ANSWERED)).map(|m| m.below),
            Some(5),
            "shell banner, help line and two shell prompts sit below it"
        );
        assert!(
            rule.live_match(&Screen::new(ANSWERED)).is_none(),
            "a prompt with a live shell under it must never be answered again"
        );
    }

    #[test]
    fn tail_distance_uses_the_last_match_not_the_first() {
        // herdr reports the FIRST matching line for an event, but when a prompt
        // repeats on screen the live one is the most recent.
        let screen = "Password:\nLogin incorrect\nPassword:\nstatus line";
        let rule = compiled("^Password:", Some(2));
        assert_eq!(
            rule.last_match(&Screen::new(screen)).map(|m| m.below),
            Some(1)
        );
        assert!(rule.live_match(&Screen::new(screen)).is_some());
    }

    #[test]
    fn output_above_an_unanswered_prompt_does_not_make_it_new() {
        // The safety property. A clock, a scrolling log or a refreshed block
        // above a prompt that is STILL WAITING must never look like a new
        // prompt, or the action runs again - re-submitting a password.
        let rule = compiled("^Password:", Some(2));
        let before = rule
            .live_match(&Screen::new("status 10:00:00\nPassword:"))
            .expect("prompt is live");
        let after = rule
            .live_match(&Screen::new("status 10:00:01\nPassword:"))
            .expect("prompt is still live");

        assert_eq!(before.below, 0, "the prompt is the tail in both");
        assert_eq!(
            before.signature(),
            after.signature(),
            "only the clock above it changed: this is the same unanswered prompt"
        );
    }

    #[test]
    fn a_reprompt_with_identical_text_is_deliberately_not_distinguished() {
        // A snapshot cannot separate "new prompt, same text" from "same prompt
        // still there", so this daemon errs towards not re-sending. Re-arming
        // happens when the tail moves on - see the two tests below.
        let rule = compiled("^login: *([^ A-Za-z0-9]|$)", Some(2));
        let first = rule.live_match(&Screen::new("banner\nlogin:")).unwrap();
        let again = rule
            .live_match(&Screen::new("banner\nlogin: someuser\nlogin:"))
            .unwrap();
        assert_eq!(
            first.signature(),
            again.signature(),
            "identical prompt text is treated as the same prompt, on purpose"
        );
    }

    #[test]
    fn the_tail_moving_on_re_arms_the_rule() {
        // Answering a prompt puts the reply on the line, so the rule no longer
        // matches at the tail at all - that is what clears the latch.
        let rule = compiled("^login: *([^ A-Za-z0-9]|$)", Some(2));
        assert!(
            rule.live_match(&Screen::new("banner\nlogin:")).is_some(),
            "the bare prompt matches"
        );
        assert!(
            rule.live_match(&Screen::new("banner\nlogin: someuser"))
                .is_none(),
            "an answered prompt does not match, which re-arms the rule"
        );
    }

    #[test]
    fn a_different_prompt_is_a_different_occurrence() {
        let rule = compiled("^(login|Password):", None);
        let login = rule.live_match(&Screen::new("login:")).unwrap();
        let password = rule.live_match(&Screen::new("Password:")).unwrap();
        assert_ne!(
            login.signature(),
            password.signature(),
            "a rule matching both prompts must act on each"
        );
    }

    #[test]
    fn the_same_prompt_still_sitting_there_keeps_its_signature() {
        let rule = compiled("^Password:", Some(2));
        let screen = "banner\nPassword:";
        let a = rule.live_match(&Screen::new(screen)).unwrap().signature();
        let b = rule.live_match(&Screen::new(screen)).unwrap().signature();
        assert_eq!(a, b, "an unchanged screen must not look like a new prompt");
    }

    #[test]
    fn blank_lines_do_not_push_a_prompt_away_from_the_tail() {
        let rule = compiled("^Password:", Some(1));
        assert!(rule
            .live_match(&Screen::new("Password:\n\n   \n\n"))
            .is_some());
    }

    #[test]
    fn a_rule_without_tail_within_matches_anywhere() {
        let rule = compiled("^Password:", None);
        assert!(rule.live_match(&Screen::new(ANSWERED)).is_some());
    }

    #[test]
    fn tail_within_declines_when_there_is_no_screen_to_judge() {
        let rule = compiled("^Password:", Some(2));
        assert!(
            rule.live_match(&Screen::new("")).is_none(),
            "without a screen a credential rule must fail safe"
        );
    }

    fn send(text: &str) -> Action {
        Action::SendText { text: text.into() }
    }

    #[test]
    fn rule_id_is_stable_for_the_same_behaviour() {
        let first = rule_id("login:", &send("admin\n"));
        let second = rule_id("login:", &send("admin\n"));
        assert_eq!(first, second);
    }

    #[test]
    fn rule_id_changes_when_the_regex_or_action_changes() {
        let base = rule_id("login:", &send("admin\n"));
        assert_ne!(base, rule_id("Login:", &send("admin\n")));
        assert_ne!(base, rule_id("login:", &send("root\n")));
        assert_ne!(
            base,
            rule_id(
                "login:",
                &Action::Notify {
                    title: "admin\n".into(),
                    body: None,
                    sound: None,
                    position: None,
                    desktop: None,
                }
            )
        );
    }

    #[test]
    fn rule_id_survives_reordering_and_unrelated_edits() {
        // The identity is the rule's own behaviour, so a rule that moved from
        // the top of the file to the bottom keeps its ledger entry.
        let before = [
            rule_id("login:", &send("admin\n")),
            rule_id("Password:", &send("hunter2\n")),
        ];
        let after = [
            rule_id("Password:", &send("hunter2\n")),
            rule_id("motd", &Action::TabMark { marker: "!".into() }),
            rule_id("login:", &send("admin\n")),
        ];
        assert!(after.contains(&before[0]));
        assert!(after.contains(&before[1]));
    }

    #[test]
    fn ledger_persists_across_restarts_and_resets_on_demand() {
        let dir =
            std::env::temp_dir().join(format!("herdr-triggers-ledger-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("once.json");
        let _ = std::fs::remove_file(&path);

        let mut ledger = Ledger::load(&path);
        ledger.mark_fired("abc", "w1:p1");
        assert!(ledger.has_fired("abc", "w1:p1"));

        let reloaded = Ledger::load(&path);
        assert!(
            reloaded.has_fired("abc", "w1:p1"),
            "a daemon restart must not re-arm a once rule"
        );
        assert!(
            !reloaded.has_fired("abc", "w1:p2"),
            "other panes stay armed"
        );

        let mut reloaded = reloaded;
        reloaded.reset();
        assert!(Ledger::load(&path).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retain_panes_drops_entries_for_vanished_terminals() {
        let mut ledger = Ledger::in_memory();
        ledger.mark_fired("abc", "term_1");
        ledger.mark_fired("abc", "term_2");
        let live: std::collections::HashSet<&str> = ["term_2"].into_iter().collect();
        ledger.retain_panes(&live);
        assert!(
            !ledger.has_fired("abc", "term_1"),
            "the gone terminal is dropped"
        );
        assert!(ledger.has_fired("abc", "term_2"), "the live terminal stays");
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn identical_text_inside_the_cooldown_is_one_occurrence() {
        let mut guard = FireGuard::new(1500, 8);
        let start = Instant::now();
        assert!(guard.would_allow_at("r", "w1:p1", "login:", start));
        guard.record_at("r", "w1:p1", "login:", start);
        assert!(!guard.would_allow_at("r", "w1:p1", "login:", start + Duration::from_millis(100)));
        assert!(
            guard.would_allow_at("r", "w1:p1", "login:", start + Duration::from_millis(1600)),
            "a genuine second prompt after the cooldown still fires"
        );
    }

    #[test]
    fn different_text_fires_without_waiting_for_the_cooldown() {
        let mut guard = FireGuard::new(1500, 8);
        let start = Instant::now();
        guard.record_at("r", "w1:p1", "login:", start);
        assert!(
            guard.would_allow_at("r", "w1:p1", "Password:", start + Duration::from_millis(50)),
            "different text is a different prompt and must not wait"
        );
    }

    #[test]
    fn cooldown_is_tracked_per_pane() {
        let mut guard = FireGuard::new(1500, 8);
        let start = Instant::now();
        guard.record_at("r", "w1:p1", "login:", start);
        assert!(
            guard.would_allow_at("r", "w1:p2", "login:", start),
            "another pane's prompt is unrelated"
        );
    }

    #[test]
    fn write_bursts_are_capped_per_second() {
        let mut guard = FireGuard::new(0, 8);
        let start = Instant::now();
        for index in 0..8 {
            let at = start + Duration::from_millis(index);
            assert!(
                guard.would_allow_write_at("r", at),
                "the first 8 writes in a second are allowed"
            );
            guard.record_write_at("r", at);
        }
        assert!(
            !guard.would_allow_write_at("r", start + Duration::from_millis(9)),
            "the 9th write in the same second is dropped"
        );
        assert!(
            guard.would_allow_write_at("r", start + Duration::from_millis(1100)),
            "the window slides"
        );
    }
}
