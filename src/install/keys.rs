//! The mod's bindings in the app's `keys.json`, merged under `global`
//! without touching anyone else's: a key bound elsewhere is never taken
//! over, and uninstall removes only keys that still open this mod.
//!
//! The file is the app's whole keymap, and the app writes it on first run,
//! so this only ever edits an existing file; creating one would leave the
//! app with two bindings and none of its defaults.

use serde_json::{Map, Value};

/// The bindings this mod ships, in `keys.json` order.
pub fn shipped(text: &str) -> Result<Vec<(String, String)>, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| format!("shipped keys.json is invalid: {e}"))?;
    let global = value
        .get("global")
        .and_then(Value::as_object)
        .ok_or("shipped keys.json has no global object")?;
    global
        .iter()
        .map(|(key, action)| {
            action
                .as_str()
                .map(|action| (key.clone(), action.to_string()))
                .ok_or_else(|| format!("shipped binding {key:?} is not a string"))
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyStatus {
    Add,
    Unchanged,
    /// Bound to another action, which is kept.
    Conflict(String),
    /// Free in `global`, but bound to another action in a section that
    /// wins over it (`terminal`, `layout:<name>`), so adding it would do
    /// nothing there.
    Shadowed {
        action: String,
        section: String,
    },
    Remove,
    /// Uninstall: not bound to this mod (absent, or bound elsewhere).
    Absent,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Nothing,
    Write(String),
}

#[derive(Debug)]
pub struct KeysPlan {
    pub keys: Vec<(String, KeyStatus)>,
    pub outcome: Outcome,
}

fn parse(text: &str) -> Result<Map<String, Value>, String> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(root)) => Ok(root),
        Ok(_) => Err("keys.json is not a JSON object".to_string()),
        Err(e) => Err(format!("keys.json is not valid JSON ({e})")),
    }
}

fn global_of(root: &Map<String, Value>) -> Result<Option<&Map<String, Value>>, String> {
    match root.get("global") {
        None => Ok(None),
        Some(Value::Object(global)) => Ok(Some(global)),
        Some(_) => Err("keys.json's \"global\" is not an object".to_string()),
    }
}

/// `secondary` in GPUI: the platform's usual shortcut modifier.
#[cfg(target_os = "macos")]
const SECONDARY: &str = "cmd";
#[cfg(not(target_os = "macos"))]
const SECONDARY: &str = "ctrl";

/// A chord in one spelling: lowercase, modifier aliases folded, modifiers
/// sorted. The app parses chords with GPUI's keystroke parser, which takes
/// modifiers in any order, reads `super` and `win` as `cmd`, and
/// `secondary` as `cmd` on macOS and `ctrl` elsewhere. `option` and
/// `control` are folded too, so a spelling the app may not accept still
/// counts as taken rather than be bound over. The key itself may be `-`
/// (`ctrl--`).
pub fn normalize(chord: &str) -> String {
    let lower = chord.to_lowercase();
    let (mods, key) = if lower == "-" {
        ("", "-")
    } else if let Some(mods) = lower.strip_suffix("--") {
        (mods, "-")
    } else {
        match lower.rsplit_once('-') {
            Some((mods, key)) => (mods, key),
            None => ("", lower.as_str()),
        }
    };
    let mut mods: Vec<&str> = mods
        .split('-')
        .filter(|m| !m.is_empty())
        .map(|m| match m {
            "option" => "alt",
            "control" => "ctrl",
            "super" | "win" => "cmd",
            "secondary" => SECONDARY,
            other => other,
        })
        .collect();
    mods.sort_unstable();
    mods.dedup();
    mods.push(key);
    mods.join("-")
}

/// The bindings in `section` for the same chord as `key`, in file order,
/// with their own spelling.
fn bound_to<'a>(section: &'a Map<String, Value>, key: &str) -> Vec<(&'a String, &'a Value)> {
    let chord = normalize(key);
    section
        .iter()
        .filter(|(spelled, _)| normalize(spelled) == chord)
        .collect()
}

fn describe(action: &Value) -> String {
    match action {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// A section more specific than `global` that binds `key` to something
/// other than `action`, as (section, what it is bound to).
fn shadowing(root: &Map<String, Value>, key: &str, action: &str) -> Option<(String, String)> {
    root.iter()
        .filter(|(section, _)| *section == "terminal" || section.starts_with("layout:"))
        .find_map(|(section, bindings)| {
            bound_to(bindings.as_object()?, key)
                .into_iter()
                .find(|(_, bound)| bound.as_str() != Some(action))
                .map(|(_, bound)| (section.clone(), describe(bound)))
        })
}

fn render(root: &Map<String, Value>) -> Result<String, String> {
    // to_string_pretty indents by two spaces, as the app's own file is.
    let mut text = serde_json::to_string_pretty(root)
        .map_err(|e| format!("cannot serialize keys.json: {e}"))?;
    text.push('\n');
    Ok(text)
}

/// What install does to the existing file.
pub fn plan_install(existing: &str, ours: &[(String, String)]) -> Result<KeysPlan, String> {
    let mut root = parse(existing)?;
    let mut global = global_of(&root)?.cloned().unwrap_or_default();
    let mut keys = Vec::new();
    for (key, action) in ours {
        let existing = bound_to(&global, key);
        let other = existing
            .iter()
            .find(|(_, bound)| bound.as_str() != Some(action.as_str()))
            .map(|(_, bound)| describe(bound));
        let status = match (existing.is_empty(), other) {
            (false, Some(other)) => KeyStatus::Conflict(other),
            (false, None) => KeyStatus::Unchanged,
            (true, _) => match shadowing(&root, key, action) {
                Some((section, bound)) => KeyStatus::Shadowed {
                    action: bound,
                    section,
                },
                None => KeyStatus::Add,
            },
        };
        if status == KeyStatus::Add {
            global.insert(key.clone(), Value::String(action.clone()));
        }
        keys.push((key.clone(), status));
    }
    let outcome = if keys.iter().any(|(_, s)| *s == KeyStatus::Add) {
        root.insert("global".to_string(), Value::Object(global));
        Outcome::Write(render(&root)?)
    } else {
        Outcome::Nothing
    };
    Ok(KeysPlan { keys, outcome })
}

/// What uninstall does to `existing`.
pub fn plan_uninstall(
    existing: Option<&str>,
    ours: &[(String, String)],
) -> Result<KeysPlan, String> {
    let Some(text) = existing else {
        return Ok(KeysPlan {
            keys: ours
                .iter()
                .map(|(key, _)| (key.clone(), KeyStatus::Absent))
                .collect(),
            outcome: Outcome::Nothing,
        });
    };
    let mut root = parse(text)?;
    let mut global = global_of(&root)?.cloned().unwrap_or_default();
    let mut keys = Vec::new();
    for (key, action) in ours {
        let mine: Vec<String> = bound_to(&global, key)
            .into_iter()
            .filter(|(_, bound)| bound.as_str() == Some(action.as_str()))
            .map(|(spelled, _)| spelled.clone())
            .collect();
        for spelled in &mine {
            global.shift_remove(spelled);
        }
        let status = if mine.is_empty() {
            KeyStatus::Absent
        } else {
            KeyStatus::Remove
        };
        keys.push((key.clone(), status));
    }
    // An emptied `global` stays as `{}`: the file is the user's.
    let outcome = if !keys.iter().any(|(_, s)| *s == KeyStatus::Remove) {
        Outcome::Nothing
    } else {
        root.insert("global".to_string(), Value::Object(global));
        Outcome::Write(render(&root)?)
    };
    Ok(KeysPlan { keys, outcome })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ours() -> Vec<(String, String)> {
        vec![
            ("ctrl-alt-g".into(), "mod.open:herdr-triggers".into()),
            ("cmd-alt-g".into(), "mod.open:herdr-triggers".into()),
        ]
    }

    #[test]
    fn the_shipped_file_is_the_two_bindings_in_order() {
        assert_eq!(shipped(include_str!("../../keys.json")).unwrap(), ours());
    }

    #[test]
    fn missing_keys_are_added_after_the_users_in_order() {
        let before = "{\n  \"global\": {\n    \"z-last\": \"a\",\n    \"alt-1\": \"b\"\n  },\n  \"other\": 1\n}\n";
        let plan = plan_install(before, &ours()).unwrap();
        assert_eq!(
            plan.keys,
            vec![
                ("ctrl-alt-g".to_string(), KeyStatus::Add),
                ("cmd-alt-g".to_string(), KeyStatus::Add)
            ]
        );
        let Outcome::Write(text) = plan.outcome else {
            panic!("expected a write");
        };
        assert_eq!(
            text,
            "{\n  \"global\": {\n    \"z-last\": \"a\",\n    \"alt-1\": \"b\",\n    \"ctrl-alt-g\": \"mod.open:herdr-triggers\",\n    \"cmd-alt-g\": \"mod.open:herdr-triggers\"\n  },\n  \"other\": 1\n}\n"
        );
    }

    #[test]
    fn a_key_bound_to_our_action_is_unchanged_and_nothing_is_written() {
        let before = r#"{"global": {"ctrl-alt-g": "mod.open:herdr-triggers", "cmd-alt-g": "mod.open:herdr-triggers"}}"#;
        let plan = plan_install(before, &ours()).unwrap();
        assert!(plan.keys.iter().all(|(_, s)| *s == KeyStatus::Unchanged));
        assert_eq!(plan.outcome, Outcome::Nothing);
    }

    #[test]
    fn a_key_bound_elsewhere_is_skipped_never_overwritten() {
        let before = r#"{"global": {"ctrl-alt-g": "tab.new"}}"#;
        let plan = plan_install(before, &ours()).unwrap();
        assert_eq!(plan.keys[0].1, KeyStatus::Conflict("tab.new".into()));
        assert_eq!(plan.keys[1].1, KeyStatus::Add);
        let Outcome::Write(text) = plan.outcome else {
            panic!("expected a write");
        };
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["global"]["ctrl-alt-g"], "tab.new");
        assert_eq!(value["global"]["cmd-alt-g"], "mod.open:herdr-triggers");
    }

    #[test]
    fn a_file_without_global_gains_one() {
        let plan = plan_install("{\"terminal\": {}}", &ours()).unwrap();
        let Outcome::Write(text) = plan.outcome else {
            panic!("expected a write");
        };
        assert_eq!(
            text,
            "{\n  \"terminal\": {},\n  \"global\": {\n    \"ctrl-alt-g\": \"mod.open:herdr-triggers\",\n    \"cmd-alt-g\": \"mod.open:herdr-triggers\"\n  }\n}\n"
        );
    }

    #[test]
    fn a_key_bound_in_a_section_that_wins_is_skipped() {
        let before = r#"{
            "global": {},
            "layout:terminals": {"ctrl-alt-g": "space.next", "x": "y"},
            "terminal": {"ctrl-alt-g": "terminal", "cmd-alt-g": "mod.open:herdr-triggers"},
            "mod:editor": {"cmd-alt-g": "editor.save"}
        }"#;
        let plan = plan_install(before, &ours()).unwrap();
        assert_eq!(
            plan.keys,
            vec![
                (
                    "ctrl-alt-g".to_string(),
                    KeyStatus::Shadowed {
                        action: "space.next".into(),
                        section: "layout:terminals".into()
                    }
                ),
                ("cmd-alt-g".to_string(), KeyStatus::Add)
            ]
        );
        let Outcome::Write(text) = plan.outcome else {
            panic!("expected a write");
        };
        let value: Value = serde_json::from_str(&text).unwrap();
        assert!(value["global"].get("ctrl-alt-g").is_none());
        assert_eq!(value["global"]["cmd-alt-g"], "mod.open:herdr-triggers");

        let object = r#"{"global": {}, "terminal": {"ctrl-alt-g": {"command": "x"}}}"#;
        assert_eq!(
            plan_install(object, &ours()).unwrap().keys[0].1,
            KeyStatus::Shadowed {
                action: r#"{"command":"x"}"#.into(),
                section: "terminal".into()
            }
        );
    }

    #[test]
    fn chords_compare_by_one_spelling() {
        for (a, b) in [
            ("ctrl-alt-g", "alt-ctrl-g"),
            ("ctrl-alt-g", "Ctrl-Alt-G"),
            ("cmd-alt-g", "option-cmd-g"),
            ("cmd-alt-g", "super-alt-g"),
            ("cmd-alt-g", "alt-win-g"),
            (&format!("{SECONDARY}-alt-g"), "secondary-alt-g"),
            ("ctrl-alt-g", "control-alt-g"),
            ("ctrl--", "Ctrl--"),
            ("ctrl-shift--", "shift-ctrl--"),
            ("ctrl-alt-g", "ctrl-alt-ctrl-g"),
        ] {
            assert_eq!(normalize(a), normalize(b), "{a} vs {b}");
        }
        for (a, b) in [
            ("cmd-g", "meta-g"),
            ("cmd-g", "command-g"),
            ("ctrl-alt-g", "ctrl-g"),
            ("ctrl--", "ctrl-="),
            ("alt-g", "g"),
            ("ctrl--", "-"),
        ] {
            assert_ne!(normalize(a), normalize(b), "{a} vs {b}");
        }
        assert_eq!(normalize("-"), "-");
        assert_eq!(normalize("Escape"), "escape");
        assert_eq!(normalize("shift-ctrl--"), "ctrl-shift--");
    }

    #[test]
    fn another_spelling_of_the_chord_is_seen_everywhere() {
        let taken =
            r#"{"global": {"alt-ctrl-g": "tab.new", "option-cmd-g": "mod.open:herdr-triggers"}}"#;
        let plan = plan_install(taken, &ours()).unwrap();
        assert_eq!(
            plan.keys,
            vec![
                (
                    "ctrl-alt-g".to_string(),
                    KeyStatus::Conflict("tab.new".into())
                ),
                ("cmd-alt-g".to_string(), KeyStatus::Unchanged)
            ]
        );
        assert_eq!(plan.outcome, Outcome::Nothing);

        let shadowed = r#"{"global": {}, "terminal": {"Alt-Ctrl-G": "terminal"}}"#;
        assert_eq!(
            plan_install(shadowed, &ours()).unwrap().keys[0].1,
            KeyStatus::Shadowed {
                action: "terminal".into(),
                section: "terminal".into()
            }
        );

        let undo = plan_uninstall(Some(taken), &ours()).unwrap();
        assert_eq!(
            undo.keys,
            vec![
                ("ctrl-alt-g".to_string(), KeyStatus::Absent),
                ("cmd-alt-g".to_string(), KeyStatus::Remove)
            ]
        );
        assert_eq!(
            undo.outcome,
            Outcome::Write("{\n  \"global\": {\n    \"alt-ctrl-g\": \"tab.new\"\n  }\n}\n".into())
        );
    }

    #[test]
    fn invalid_json_is_an_error_for_both_directions() {
        for bad in ["{\"global\": ", "[1]", r#"{"global": 3}"#] {
            assert!(plan_install(bad, &ours()).is_err(), "{bad}");
            assert!(plan_uninstall(Some(bad), &ours()).is_err(), "{bad}");
        }
    }

    #[test]
    fn uninstall_removes_only_keys_that_still_open_this_mod() {
        let before = r#"{"global": {"alt-1": "x", "ctrl-alt-g": "mod.open:herdr-triggers", "cmd-alt-g": "tab.new"}}"#;
        let plan = plan_uninstall(Some(before), &ours()).unwrap();
        assert_eq!(
            plan.keys,
            vec![
                ("ctrl-alt-g".to_string(), KeyStatus::Remove),
                ("cmd-alt-g".to_string(), KeyStatus::Absent)
            ]
        );
        let Outcome::Write(text) = plan.outcome else {
            panic!("expected a write");
        };
        assert_eq!(
            text,
            "{\n  \"global\": {\n    \"alt-1\": \"x\",\n    \"cmd-alt-g\": \"tab.new\"\n  }\n}\n"
        );
    }

    #[test]
    fn uninstall_of_a_file_holding_only_ours_keeps_an_empty_global() {
        let Outcome::Write(created) = plan_install("{\"global\": {}}", &ours()).unwrap().outcome
        else {
            panic!("expected a write");
        };
        assert_eq!(
            plan_uninstall(Some(&created), &ours()).unwrap().outcome,
            Outcome::Write("{\n  \"global\": {}\n}\n".to_string())
        );
        assert_eq!(
            plan_uninstall(None, &ours()).unwrap().outcome,
            Outcome::Nothing
        );
    }
}
