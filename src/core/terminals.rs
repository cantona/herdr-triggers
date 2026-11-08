//! The terminals herdr-core knows, kept current from its event stream.
//!
//! `events.subscribe` answers with a snapshot and then pushes every fact
//! change, so the daemon never lists terminals in its poll loop: a poll is
//! one `terminal.read` per watched pane and nothing else. Only local live
//! terminals are projected; a remote one (`mac/t7`) would turn every read
//! into an ssh round trip.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::server::PaneInfo;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    pub address: String,
    /// The program's pid. herdr-core restarts a lost terminal under the same
    /// address, so the once-ledger keys on address and pid together.
    pub pid: Option<u64>,
    pub live: bool,
    pub title: String,
    pub cols: u16,
    pub rows: u16,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
pub struct Table {
    terminals: BTreeMap<String, Terminal>,
    /// The snapshot's sequence number: an event at or below it is already
    /// in the snapshot, or older than it (herdr-core says to drop those
    /// after a `missed` snapshot).
    after: u64,
}

impl Table {
    pub fn from_snapshot(seq: u64, terminals: &[Value]) -> Self {
        let mut table = Self {
            after: seq,
            ..Self::default()
        };
        for record in terminals {
            table.upsert(record);
        }
        table
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.terminals.len()
    }

    /// One notification line from the event channel. `event` updates one
    /// terminal from its record (or the fields the event carries), `missed`
    /// replaces the table with the snapshot it brings, `heartbeat` is noise.
    pub fn apply(&mut self, notification: &Value) {
        let params = &notification["params"];
        match notification["method"].as_str() {
            Some("missed") => {
                if let Some(terminals) = params["terminals"].as_array() {
                    *self = Self::from_snapshot(params["seq"].as_u64().unwrap_or(0), terminals);
                }
            }
            Some("event") => {
                let Some(address) = params["terminal"].as_str() else {
                    return;
                };
                if params["seq"].as_u64().is_some_and(|seq| seq <= self.after) {
                    return;
                }
                match params["type"].as_str() {
                    Some("removed") => {
                        self.terminals.remove(address);
                    }
                    Some("created") | Some("restarted") => {
                        if params["data"]["record"].is_object() {
                            self.upsert(&params["data"]["record"]);
                        } else if params["data"].is_object() {
                            self.upsert(&params["data"]);
                        }
                    }
                    Some("finished") | Some("lost") => {
                        if let Some(t) = self.terminals.get_mut(address) {
                            t.live = false;
                        }
                    }
                    Some("title") => {
                        if let Some(t) = self.terminals.get_mut(address) {
                            t.title = params["data"]["title"]
                                .as_str()
                                .or(params["data"].as_str())
                                .unwrap_or_default()
                                .to_string();
                        }
                    }
                    Some("labels") => {
                        if let Some(t) = self.terminals.get_mut(address) {
                            t.labels = labels_of(&params["data"]["labels"])
                                .or_else(|| labels_of(&params["data"]))
                                .unwrap_or_default();
                        }
                    }
                    Some("size") => {
                        if let Some(t) = self.terminals.get_mut(address) {
                            let data = &params["data"];
                            if let (Some(cols), Some(rows)) =
                                (data["cols"].as_u64(), data["rows"].as_u64())
                            {
                                t.cols = cols as u16;
                                t.rows = rows as u16;
                            }
                        }
                    }
                    // A full record rides along on any event whose terminal is
                    // new to this subscription; take it when present.
                    _ => {
                        if params["data"]["record"].is_object() {
                            self.upsert(&params["data"]["record"]);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn upsert(&mut self, record: &Value) {
        let Some(address) = record["address"].as_str() else {
            return;
        };
        let terminal = Terminal {
            address: address.to_string(),
            pid: record["pid"].as_u64(),
            live: record["state"].as_str() == Some("live"),
            title: record["title"].as_str().unwrap_or_default().to_string(),
            cols: record["cols"].as_u64().unwrap_or(80) as u16,
            rows: record["rows"].as_u64().unwrap_or(24) as u16,
            labels: labels_of(&record["labels"]).unwrap_or_default(),
        };
        self.terminals.insert(terminal.address.clone(), terminal);
    }

    /// Local live terminals as the engine sees panes. Titles: the terminal's
    /// title, then the space and tab names from the `where` label
    /// (`"<space> / <tab>"`), then `where` whole, so a `pane_title` scope can
    /// match any of them.
    pub fn panes(&self) -> Vec<PaneInfo> {
        self.terminals
            .values()
            .filter(|t| t.live && !t.address.contains('/'))
            .map(|t| {
                let mut titles = Vec::with_capacity(4);
                if !t.title.is_empty() {
                    titles.push(t.title.clone());
                }
                if let Some(place) = t.labels.get("where") {
                    if let Some((space, tab)) = place.split_once(" / ") {
                        titles.push(space.to_string());
                        titles.push(tab.to_string());
                    }
                    titles.push(place.clone());
                }
                PaneInfo {
                    pane_id: t.address.clone(),
                    terminal_id: match t.pid {
                        Some(pid) => format!("{}@{pid}", t.address),
                        None => t.address.clone(),
                    },
                    tab_id: t.labels.get("herdr.tab").cloned().unwrap_or_default(),
                    workspace_id: t.labels.get("herdr.space").cloned().unwrap_or_default(),
                    titles,
                }
            })
            .collect()
    }

    pub fn size(&self, address: &str) -> Option<(u16, u16)> {
        self.terminals.get(address).map(|t| (t.cols, t.rows))
    }

    /// The tab's name: the tab half of `where` on any terminal in that tab.
    pub fn tab_label(&self, tab_id: &str) -> Option<String> {
        self.terminals
            .values()
            .filter(|t| t.labels.get("herdr.tab").is_some_and(|id| id == tab_id))
            .find_map(|t| {
                t.labels
                    .get("where")
                    .and_then(|w| w.split_once(" / "))
                    .map(|(_, tab)| tab.to_string())
            })
    }
}

fn labels_of(value: &Value) -> Option<BTreeMap<String, String>> {
    let map = value.as_object()?;
    Some(
        map.iter()
            .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A record as herdr-core 0.1.19 returns it from `terminal.list`.
    pub(crate) fn record(address: &str, title: &str, space: &str, tab: &str, place: &str) -> Value {
        json!({
            "address": address, "argv": ["bash", "-l"], "cols": 191, "rows": 42,
            "created_ms": 1794212049221u64, "cwd": "/home/u", "die_with_parent": false,
            "exit_code": null, "finished_ms": null,
            "foreground": {"argv": ["bash", "-l"], "cwd": "/home/u", "name": "bash", "pid": 1559557},
            "identity": "ghostty", "keep": true,
            "labels": {"herdr.seen": "0", "herdr.space": space, "herdr.tab": tab, "where": place},
            "name": null, "notify": true, "parent": null, "pid": 1559557, "reason": null,
            "reported": false, "rm": false, "screen": "exact", "signal": null, "state": "live",
            "status": null, "status_ms": null, "template": null, "title": title, "turn": 0
        })
    }

    #[test]
    fn a_snapshot_projects_local_live_terminals_with_every_name_a_scope_may_match() {
        let mut finished = record("t2", "done", "sp1", "tb1", "~ / 1");
        finished["state"] = json!("finished");
        let remote = record("mac/t7", "remote", "sp9", "tb9", "repo / 2");
        let table = Table::from_snapshot(
            0,
            &[
                record("t13", "cantona@yin: ~", "sp1", "tb1", "~ / 1"),
                finished,
                remote,
            ],
        );
        assert_eq!(table.len(), 3);
        let panes = table.panes();
        assert_eq!(panes.len(), 1);
        let pane = &panes[0];
        assert_eq!(pane.pane_id, "t13");
        // The ledger key carries the pid: a restarted terminal keeps its
        // address but runs a new program, and must be re-armed.
        assert_eq!(pane.terminal_id, "t13@1559557");
        assert_eq!(pane.tab_id, "tb1");
        assert_eq!(pane.workspace_id, "sp1");
        assert_eq!(pane.titles, vec!["cantona@yin: ~", "~", "1", "~ / 1"]);
        assert_eq!(table.size("t13"), Some((191, 42)));
        assert_eq!(table.tab_label("tb1"), Some("1".to_string()));
        assert_eq!(table.tab_label("tb-none"), None);
    }

    #[test]
    fn missing_labels_leave_ids_empty_rather_than_dropping_the_terminal() {
        let mut bare = record("t1", "", "", "", "");
        bare["labels"] = json!({});
        let panes = Table::from_snapshot(0, &[bare]).panes();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].tab_id, "");
        assert_eq!(panes[0].workspace_id, "");
        assert!(panes[0].titles.is_empty());
    }

    fn event(kind: &str, terminal: &str, data: Value) -> Value {
        json!({"jsonrpc": "2.0", "method": "event", "params": {"type": kind, "terminal": terminal, "data": data, "seq": 7}})
    }

    #[test]
    fn events_update_the_table_in_place() {
        let mut table = Table::from_snapshot(0, &[record("t1", "one", "sp1", "tb1", "~ / 1")]);
        table.apply(&event(
            "created",
            "t2",
            json!({"record": record("t2", "two", "sp1", "tb2", "~ / 2")}),
        ));
        assert_eq!(table.panes().len(), 2);
        table.apply(&event("title", "t2", json!({"title": "vim notes"})));
        assert_eq!(table.panes()[1].titles[0], "vim notes");
        table.apply(&event(
            "labels",
            "t2",
            json!({"labels": {"herdr.space": "sp1", "herdr.tab": "tb2", "where": "~ / fix login"}}),
        ));
        assert_eq!(table.tab_label("tb2"), Some("fix login".to_string()));
        table.apply(&event("size", "t2", json!({"cols": 100, "rows": 30})));
        assert_eq!(table.size("t2"), Some((100, 30)));
        table.apply(&event("finished", "t1", json!({"reason": "exited"})));
        assert_eq!(table.panes().len(), 1);
        table.apply(&event("removed", "t1", json!({})));
        assert_eq!(table.len(), 1);
        table.apply(&json!({"jsonrpc": "2.0", "method": "heartbeat", "params": {"seq": 8}}));
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn events_at_or_below_the_snapshots_seq_are_dropped() {
        let at = |kind: &str, terminal: &str, data: Value, seq: u64| json!({"jsonrpc": "2.0", "method": "event", "params": {"type": kind, "terminal": terminal, "data": data, "seq": seq}});
        let mut table = Table::from_snapshot(0, &[record("t1", "one", "sp1", "tb1", "~ / 1")]);
        table.apply(&json!({"jsonrpc": "2.0", "method": "missed", "params": {"seq": 40, "terminals": [record("t1", "one", "sp1", "tb1", "~ / new")]}}));
        // Queued before the snapshot: an old name and a terminal since removed.
        table.apply(&at(
            "labels",
            "t1",
            json!({"labels": {"herdr.space": "sp1", "herdr.tab": "tb1", "where": "~ / old"}}),
            39,
        ));
        table.apply(&at(
            "created",
            "t2",
            json!({"record": record("t2", "two", "sp1", "tb2", "~ / 2")}),
            40,
        ));
        assert_eq!(table.tab_label("tb1"), Some("new".to_string()));
        assert_eq!(table.len(), 1);
        table.apply(&at(
            "created",
            "t2",
            json!({"record": record("t2", "two", "sp1", "tb2", "~ / 2")}),
            41,
        ));
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn missed_replaces_the_table_with_the_fresh_snapshot() {
        let mut table = Table::from_snapshot(
            0,
            &[
                record("t1", "one", "sp1", "tb1", "~ / 1"),
                record("t2", "two", "sp1", "tb1", "~ / 1"),
            ],
        );
        table.apply(&json!({"jsonrpc": "2.0", "method": "missed", "params": {"seq": 40, "terminals": [record("t3", "three", "sp2", "tb3", "repo / 1")]}}));
        assert_eq!(table.len(), 1);
        assert_eq!(table.panes()[0].pane_id, "t3");
    }
}
