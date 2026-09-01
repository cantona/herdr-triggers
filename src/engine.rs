//! The resident loop: keep rules armed on the panes they apply to, match them
//! against each pane's screen, and run the action when one hits.
//!
//! The daemon reads each watched pane's screen on a fixed interval and runs the
//! rules itself. It deliberately does NOT use herdr's `pane.output_matched`
//! subscription: that reports a match only on a rising edge of "text present
//! anywhere on screen", which never falls while a console sits at its prompt,
//! and resetting it means tearing down and rebuilding the connection. Matching
//! here instead gives one poll interval of latency, an edge this daemon
//! defines (the text being at the screen tail), and room for the guards that
//! stop a rule firing on its own output.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{json, Value};

use crate::actions::{self, Context};
use crate::client::Client;
use crate::config::{self, Settings, TriggersConfig};
use crate::rules::{CompiledRule, FireGuard, Ledger, Screen};
use crate::{log_detail, log_line};

/// Each poll costs three list calls plus one screen read per watched pane, so
/// an unscoped rule set on a big session is capped rather than allowed to
/// hammer herdr's app thread.
const MAX_WATCHED_PANES: usize = 32;

/// First wait after a failed action, doubling per consecutive failure up to
/// `MAX_RETRY_BACKOFF`.
const RETRY_BACKOFF: Duration = Duration::from_secs(2);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(60);

pub static STOP: AtomicBool = AtomicBool::new(false);
pub static RELOAD: AtomicBool = AtomicBool::new(false);
pub static RESET: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub pane_id: String,
    /// Unique per pane instance and stable for its life. Unlike `pane_id`
    /// (a per-workspace counter herdr restarts from 1 each launch), this is
    /// never reused, so the persistent once-ledger keys on it: a pane that
    /// takes a recycled public id after a herdr restart does not inherit the
    /// old pane's fired state.
    pub terminal_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    /// Every name this pane can be known by: a manual `pane rename` label, an
    /// agent-set title, the terminal's own title, and the labels of its TAB and
    /// its WORKSPACE - the last two also with any `[N] ` position prefix
    /// stripped. `scope.pane_title` matches if ANY of them does, because which
    /// one carries the identity depends on what is running in the pane: a
    /// full-screen client often sets no title at all, leaving only the tab or
    /// workspace name.
    pub titles: Vec<String>,
}

impl PaneInfo {
    /// The id the persistent ledger keys on: the never-reused terminal id when
    /// herdr reports one, else the public pane id (older herdr, or a pane still
    /// reported without the field).
    fn ledger_id(&self) -> &str {
        if self.terminal_id.is_empty() {
            &self.pane_id
        } else {
            &self.terminal_id
        }
    }
}

pub struct Engine {
    client: Client,
    triggers_path: PathBuf,
    secrets_path: PathBuf,
    rules: Vec<CompiledRule>,
    settings: Settings,
    ledger: Ledger,
    guard: FireGuard,
    /// Per (rule, pane): the signature of the match last acted on, so a prompt
    /// is answered once. The signature is the matched line's text alone, so a
    /// later prompt carrying the SAME text is treated as that same prompt and
    /// is NOT answered again - see `Match::signature` for why that direction
    /// is deliberate. Cleared when the rule stops matching at the tail.
    latched: HashMap<String, u64>,
    /// Every (rule, pane) ever evaluated, so a rule can tell a prompt that was
    /// already on screen at startup from one that appeared later.
    known: HashSet<String>,
    /// Signature of the last match reported as too far from the tail, so the
    /// explanation is logged once per stale prompt and not once per poll.
    reported_stale: HashMap<String, u64>,
    /// Panes whose read failure has already been logged.
    read_failures: std::cell::RefCell<HashSet<String>>,
    /// Signature last reported as held by the cooldown, so that too is logged
    /// once rather than every poll.
    reported_held: HashMap<String, u64>,
    /// Consecutive failures per (rule, pane), and the earliest time to try
    /// again. A rule left armed after a failure must not retry at poll rate.
    retries: HashMap<String, u32>,
    retry_after: HashMap<String, std::time::Instant>,
}

impl Engine {
    pub fn new(client: Client, config_dir: &Path, state_dir: &Path) -> Self {
        let triggers_path = config_dir.join("triggers.toml");
        let secrets_path = config_dir.join("secrets.toml");
        let ledger = Ledger::load(&state_dir.join("once-ledger.json"));
        let settings = Settings::default();
        let guard = FireGuard::new(settings.cooldown_ms, settings.max_writes_per_second);
        let mut engine = Self {
            client,
            triggers_path,
            secrets_path,
            rules: Vec::new(),
            settings,
            ledger,
            guard,
            latched: HashMap::new(),
            known: HashSet::new(),
            reported_stale: HashMap::new(),
            read_failures: std::cell::RefCell::new(HashSet::new()),
            reported_held: HashMap::new(),
            retries: HashMap::new(),
            retry_after: HashMap::new(),
        };
        engine.reload();
        engine
    }

    /// A bad config never takes the daemon down: config errors are non-fatal and
    /// the previously loaded rules stay live.
    pub fn reload(&mut self) {
        let loaded = match config::load_triggers(&self.triggers_path) {
            Ok(loaded) => loaded,
            Err(err) => {
                log_line!(
                    "config error, keeping {} live rules: {err}",
                    self.rules.len()
                );
                return;
            }
        };
        let TriggersConfig { settings, rules } = loaded;
        // FIRST, before anything in this function logs: a reload is the only
        // place the level can change, and every line below - the bad-regex
        // notes, the warnings (which quote the rule's regex) and the "loaded"
        // summary - must already be subject to the new setting. Applying it
        // afterwards let `log = "off"` still write on every reload.
        if let Some(level) = crate::log::Level::parse(&settings.log) {
            crate::log::set_level(level);
        }
        let mut compiled = Vec::with_capacity(rules.len());
        for (index, rule) in rules.iter().enumerate() {
            match CompiledRule::compile(rule) {
                Ok(rule) => compiled.push(rule),
                Err(err) => log_line!("rule {index} skipped, bad regex: {err}"),
            }
        }
        for warning in config::warnings(&rules) {
            log_line!("warning: {warning}");
        }
        log_line!(
            "loaded {} rules (cooldown {}ms, max {} writes/s)",
            compiled.len(),
            settings.cooldown_ms,
            settings.max_writes_per_second
        );
        self.guard = FireGuard::new(settings.cooldown_ms, settings.max_writes_per_second);
        self.settings = settings;
        self.rules = compiled;
        // A reload re-arms every rule: the latch describes the old rule set.
        // `known` is deliberately kept - clearing it would make the prompt
        // currently on screen look like startup text and get suppressed, which
        // is the opposite of re-arming.
        self.latched.clear();
    }

    pub fn reset_ledger(&mut self) {
        let cleared = self.ledger.len();
        self.ledger.reset();
        // Drop the latch so the prompt on screen is answered again, but keep
        // `known`: an explicit reset means "fire again", and re-seeding the
        // startup suppression would silently do the reverse.
        self.latched.clear();
        log_line!("reset: re-armed {cleared} once entries");
    }

    pub fn run(&mut self) {
        let mut backoff = INITIAL_BACKOFF;
        let mut announced: Option<(usize, usize)> = None;
        let mut capped = false;
        while !STOP.load(Ordering::Relaxed) {
            self.drain_control_flags();

            let panes = match self.panes() {
                Ok(panes) => panes,
                Err(err) => {
                    log_line!("cannot reach herdr ({err}); retrying");
                    announced = None;
                    sleep_interruptibly(backoff);
                    backoff = next_backoff(backoff);
                    continue;
                }
            };
            backoff = INITIAL_BACKOFF;

            // Drop state for terminals that are gone. The ledger keys on
            // terminal ids (restart-safe), the in-memory guards on pane ids.
            let live_terminals: HashSet<&str> = panes.iter().map(PaneInfo::ledger_id).collect();
            let live_panes: HashSet<&str> =
                panes.iter().map(|pane| pane.pane_id.as_str()).collect();
            self.ledger.retain_panes(&live_terminals);
            self.guard.retain_panes(&live_panes);
            let alive = |key: &String| {
                key.split('\t')
                    .nth(1)
                    .is_some_and(|pane| live_panes.contains(pane))
            };
            self.latched.retain(|key, _| alive(key));
            self.known.retain(alive);
            self.reported_stale.retain(|key, _| alive(key));
            self.reported_held.retain(|key, _| alive(key));
            self.retries.retain(|key, _| alive(key));
            self.retry_after.retain(|key, _| alive(key));
            self.read_failures
                .borrow_mut()
                .retain(|pane| live_panes.contains(pane.as_str()));

            // Panes a SCOPED rule wants come first. The cap exists to bound
            // an unscoped rule set, and must never spend the budget on a pane
            // matched only by a broad rule while starving one a credential rule
            // was aimed at.
            let mut ordered: Vec<&PaneInfo> = panes.iter().collect();
            ordered.sort_by_key(|pane| {
                let targeted = self.rules.iter().any(|rule| {
                    rule.scope.is_some()
                        && rule.applies_to(&pane.pane_id, &pane.workspace_id, &pane.titles)
                });
                !targeted
            });

            let mut watched = 0usize;
            let mut watched_panes = 0usize;
            let mut over_cap = false;
            for pane in ordered {
                let applicable: Vec<usize> = (0..self.rules.len())
                    .filter(|index| {
                        self.rules[*index].applies_to(
                            &pane.pane_id,
                            &pane.workspace_id,
                            &pane.titles,
                        )
                    })
                    .collect();
                if applicable.is_empty() {
                    continue;
                }
                watched_panes += 1;
                if watched_panes > MAX_WATCHED_PANES {
                    over_cap = true;
                    break;
                }
                watched += applicable.len();
                // One read per pane per poll, and one pass over its lines,
                // both shared by every rule on that pane.
                let text = self.read_pane(&pane.pane_id);
                let screen = Screen::new(&text);
                // A blank screen is still evaluated: it means the pane was
                // cleared, nothing matches at the tail, and every rule on it
                // must drop its latch - otherwise `clear` could never re-arm a
                // rule. Skipping this was why a cleared prompt stayed stuck.
                for index in applicable {
                    self.evaluate(index, pane, &text, &screen);
                }
            }
            if over_cap != capped {
                if over_cap {
                    log_line!(
                        "more than {MAX_WATCHED_PANES} panes match a rule; the rest are not \
                         watched - add a scope"
                    );
                }
                capped = over_cap;
            }
            // Report on change, not once: panes come and go and scopes start
            // matching, and a stale "watching N" line is misleading.
            if announced != Some((watched, panes.len())) {
                log_line!(
                    "watching {watched} rule/pane pairs across {} panes every {}ms",
                    panes.len(),
                    self.poll_interval().as_millis()
                );
                announced = Some((watched, panes.len()));
            }

            sleep_interruptibly(self.poll_interval());
        }
        log_line!("stopping");
    }

    fn poll_interval(&self) -> Duration {
        poll_interval_for(self.settings.poll_ms)
    }

    fn drain_control_flags(&mut self) {
        if RELOAD.swap(false, Ordering::Relaxed) {
            self.reload();
        }
        if RESET.swap(false, Ordering::Relaxed) {
            self.reset_ledger();
        }
    }

    /// Tab id -> label, in one request. The names a user actually sees and
    /// types (`usb0`, `monitor`) are TAB labels, not pane attributes: a pane's
    /// own title is usually just the shell's (`user@host: ~`). A scope written
    /// against the visible name has to be able to match the tab.
    fn tab_labels(&self) -> HashMap<String, String> {
        let Ok(result) = self.client.request("tab.list", json!({})) else {
            return HashMap::new();
        };
        result
            .get("tabs")
            .and_then(Value::as_array)
            .map(|tabs| {
                tabs.iter()
                    .filter_map(|tab| {
                        let id = tab.get("tab_id")?.as_str()?.to_string();
                        let label = tab.get("label")?.as_str()?.to_string();
                        Some((id, label))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Workspace id -> label, in one request. A workspace carries a name too
    /// (`monitor`), and for a pane whose tab is just `bash` that name is the
    /// only thing identifying where it is.
    fn workspace_labels(&self) -> HashMap<String, String> {
        let Ok(result) = self.client.request("workspace.list", json!({})) else {
            return HashMap::new();
        };
        result
            .get("workspaces")
            .and_then(Value::as_array)
            .map(|spaces| {
                spaces
                    .iter()
                    .filter_map(|space| {
                        let id = space.get("workspace_id")?.as_str()?.to_string();
                        let label = space.get("label")?.as_str()?.to_string();
                        Some((id, label))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn panes(&self) -> Result<Vec<PaneInfo>, crate::client::ClientError> {
        let result = self.client.request("pane.list", json!({}))?;
        let tabs = self.tab_labels();
        let spaces = self.workspace_labels();
        let panes = result
            .get("panes")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                crate::client::ClientError::Malformed("pane.list has no panes".into())
            })?;
        Ok(panes
            .iter()
            .filter_map(|pane| {
                Some(PaneInfo {
                    pane_id: pane.get("pane_id")?.as_str()?.to_string(),
                    terminal_id: pane
                        .get("terminal_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    tab_id: pane
                        .get("tab_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    workspace_id: pane
                        .get("workspace_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    titles: {
                        let mut titles: Vec<String> = [
                            "label",
                            "title",
                            "terminal_title_stripped",
                            "terminal_title",
                        ]
                        .iter()
                        .filter_map(|field| pane.get(*field).and_then(Value::as_str))
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                        .collect();
                        // The tab's and the workspace's labels too: those are the
                        // names on screen. A pane's own title is usually just
                        // the shell's, and its tab may only say "bash".
                        let mut add = |label: &String| {
                            titles.push(label.clone());
                            // herdr decorates a label with its position
                            // ("[1] monitor"), so match the bare name as well
                            // and a scope need not track reordering.
                            if let Some(bare) = strip_tab_number(label) {
                                titles.push(bare.to_string());
                            }
                        };
                        if let Some(label) = pane
                            .get("tab_id")
                            .and_then(Value::as_str)
                            .and_then(|id| tabs.get(id))
                        {
                            add(label);
                        }
                        if let Some(label) = pane
                            .get("workspace_id")
                            .and_then(Value::as_str)
                            .and_then(|id| spaces.get(id))
                        {
                            add(label);
                        }
                        titles
                    },
                })
            })
            .collect())
    }

    fn read_pane(&self, pane_id: &str) -> String {
        let mut params = json!({
            "pane_id": pane_id,
            "source": self.settings.source,
            "strip_ansi": true,
        });
        if let Some(lines) = self.settings.lines {
            params["lines"] = json!(lines);
        }
        match self.client.request("pane.read", params) {
            Ok(result) => {
                self.read_failures.borrow_mut().remove(pane_id);
                result
                    .get("read")
                    .and_then(|read| read.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            }
            Err(err) => {
                // A failure, not noise: a pane that cannot be read never has
                // its rules evaluated at all, so this is recorded at `fires`
                // too. Deduped per pane, so it cannot flood the ring however
                // long the pane stays unreadable.
                if self.read_failures.borrow_mut().insert(pane_id.to_string()) {
                    log_line!("cannot read {pane_id}: {err}");
                }
                String::new()
            }
        }
    }

    /// One rule against one pane's current screen.
    ///
    /// The latch is edge-triggered on the rule's text being AT THE TAIL, which
    /// is this daemon's stand-in for cursor-line-only firing: a prompt waiting
    /// for input sits at the tail, and answering it pushes the match away, so
    /// the latch falls and the next prompt is a fresh edge. Nothing fires while
    /// the same prompt keeps sitting there.
    /// `text` is the raw screen, handed to a coprocess on stdin; `screen` is
    /// the same content with blank lines dropped, which is what rules match on.
    fn evaluate(&mut self, index: usize, pane: &PaneInfo, text: &str, screen: &Screen<'_>) {
        let id = self.rules[index].id.clone();
        let key = armed_key(&id, &pane.pane_id);

        // Recorded before the match test on purpose: a pair first seen with no
        // match is still "known", so a prompt appearing later is a genuine edge.
        let first_sight = self.known.insert(key.clone());

        let Some(found) = self.rules[index].live_match(screen) else {
            // Nothing at the tail: answered, or scrolled away. The next one is
            // a fresh occurrence.
            self.latched.remove(&key);
            // Say so once when the text IS on screen but too far up: silently
            // doing nothing is the hardest thing to diagnose.
            if let Some(stale) = self.rules[index].last_match(screen) {
                let signature = stale.signature();
                if self.reported_stale.insert(key.clone(), signature) != Some(signature) {
                    log_detail!(
                        "rule {id} not fired in {}: match is {} lines above the tail, outside \
                         tail_within",
                        pane.pane_id,
                        stale.below
                    );
                }
            } else {
                self.reported_stale.remove(&key);
            }
            return;
        };
        self.reported_stale.remove(&key);
        let signature = found.signature();
        let matched_line = found.line.to_string();

        // Same prompt as the last fire, so do nothing. The signature is the
        // matched line's TEXT ALONE, which means a reprompt carrying identical
        // text IS caught here and not answered a second time. That is the
        // deliberate tradeoff: a snapshot cannot tell a new prompt from the
        // same one still waiting, and re-sending a credential is worse than
        // missing an occurrence. Re-arming happens when the tail moves on -
        // the match disappears, or its text changes - see `Match::signature`.
        if self.latched.get(&key) == Some(&signature) {
            return;
        }
        // Present on the very first look: it was there before the daemon was,
        // so it cannot be told apart from a prompt already answered by hand.
        if first_sight && !self.settings.fire_on_existing_text {
            self.latched.insert(key, signature);
            log_line!(
                "rule {id} not fired in {}: already on screen at startup (fire_on_existing_text is false)",
                pane.pane_id
            );
            return;
        }

        let pane_id = pane.pane_id.as_str();
        let ledger_id = pane.ledger_id();
        let once = self.rules[index].once;
        if once && self.ledger.has_fired(&id, ledger_id) {
            self.latched.insert(key, signature);
            return;
        }
        if !self.guard.would_allow(&id, pane_id, &matched_line) {
            // Worth a line, deduped: an over-long cooldown delaying a reprompt
            // with identical text is otherwise invisible, but at poll rate it
            // would flood.
            if self.reported_held.insert(key.clone(), signature) != Some(signature) {
                log_detail!("rule {id} held in {pane_id} by cooldown_ms");
            }
            return;
        }
        if self.rules[index].action.writes_to_pane() && !self.guard.would_allow_write(&id) {
            log_line!("rule {id} hit the write-burst cap in {pane_id}; dropped");
            return;
        }
        self.reported_held.remove(&key);

        let context = Context {
            pane_id,
            tab_id: &pane.tab_id,
            matched_line: &matched_line,
            screen: text,
            secrets_path: &self.secrets_path,
        };
        // A failure that the user must fix (a secrets mode or a typo) leaves
        // the rule armed on purpose, so the retry has to be paced: the console
        // sits at the same prompt, and retrying every poll would run the action
        // ten times a second and churn the log ring.
        if let Some(next_try) = self.retry_after.get(&key) {
            if std::time::Instant::now() < *next_try {
                return;
            }
        }

        let outcome = actions::run(&self.client, &self.rules[index], &context);
        // Tokens are spent only now, after the action has actually run: a fire
        // that was never attempted must not consume the retry's budget.
        self.guard.record(&id, pane_id, &matched_line);
        if self.rules[index].action.writes_to_pane() {
            self.guard.record_write(&id);
        }
        let consume = match outcome {
            Ok(summary) => {
                // For a coprocess this means STARTED, not answered: it runs on
                // its own thread, and whether it answered, declined or timed
                // out is logged from there. A declining script therefore shows
                // up here as a start with no answer line after it.
                log_line!("rule {id} started in {pane_id}: {summary}");
                true
            }
            Err(err) => {
                let attempt = self.retries.entry(key.clone()).or_insert(0);
                *attempt = attempt.saturating_add(1);
                let wait = RETRY_BACKOFF
                    .saturating_mul(2u32.saturating_pow((*attempt - 1).min(6)))
                    .min(MAX_RETRY_BACKOFF);
                // Logged only on the first failure and then on each backoff
                // step, never once per poll.
                log_line!(
                    "rule {id} failed in {pane_id} (attempt {attempt}, next try in {}s): {err}",
                    wait.as_secs()
                );
                self.retry_after
                    .insert(key.clone(), std::time::Instant::now() + wait);
                // A failure the user must fix (an unreadable secret, a missing
                // program) leaves the rule armed so the fix takes effect on its
                // own. Anything else consumes.
                !err.is_users_to_fix()
            }
        };
        if consume {
            self.retries.remove(&key);
            self.retry_after.remove(&key);
        }
        // Only latch when there is nothing for the user to fix. Latching a
        // secrets failure would defeat the retry the ledger deliberately keeps
        // open: a waiting console never changes its screen, so the signature
        // stays the same and the action would never run again.
        if consume {
            self.latched.insert(key, signature);
        }
        if once && consume {
            self.ledger.mark_fired(&id, ledger_id);
        }
    }
}

/// The poll interval a `poll_ms` setting resolves to. Below the floor the loop
/// would spend more time issuing requests than waiting; above the ceiling a
/// prompt would sit unanswered for minutes.
fn poll_interval_for(poll_ms: u64) -> Duration {
    Duration::from_millis(poll_ms.clamp(50, 60_000))
}

/// Strips herdr's `[N] ` tab-number decoration, so a scope can match the name
/// the user gave the tab regardless of where it sits.
fn strip_tab_number(label: &str) -> Option<&str> {
    let rest = label.strip_prefix('[')?;
    let (number, rest) = rest.split_once("] ")?;
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(rest)
}

fn armed_key(rule_id: &str, pane_id: &str) -> String {
    format!("{rule_id}\t{pane_id}")
}

const INITIAL_BACKOFF: Duration = Duration::from_millis(200);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_BACKOFF)
}

/// Sleeps `total`, waking early on stop. Sleeps the exact remainder on the last
/// step so the interval is honoured rather than rounded up to a step boundary.
fn sleep_interruptibly(total: Duration) {
    const STEP: Duration = Duration::from_millis(20);
    let deadline = std::time::Instant::now() + total;
    loop {
        if STOP.load(Ordering::Relaxed) {
            return;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        std::thread::sleep(remaining.min(STEP));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_number_decoration_is_stripped_so_a_scope_can_match_the_name() {
        assert_eq!(strip_tab_number("[3] monitor"), Some("monitor"));
        assert_eq!(strip_tab_number("[12] voip fh"), Some("voip fh"));
        assert_eq!(strip_tab_number("monitor"), None, "no decoration to strip");
        assert_eq!(strip_tab_number("[x] monitor"), None, "not a tab number");
        assert_eq!(strip_tab_number("[3]monitor"), None, "needs the space");
    }

    #[test]
    fn poll_interval_is_clamped_to_a_sane_range() {
        // Calls the production helper rather than repeating the bounds, so a
        // change to them cannot pass this test unnoticed.
        assert_eq!(Settings::default().poll_ms, 200);
        assert_eq!(poll_interval_for(200), Duration::from_millis(200));
        assert_eq!(poll_interval_for(1), Duration::from_millis(50), "floor");
        assert_eq!(
            poll_interval_for(10_000_000),
            Duration::from_millis(60_000),
            "ceiling"
        );
    }
}
