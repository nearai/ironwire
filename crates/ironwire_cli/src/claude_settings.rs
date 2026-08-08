//! Editing `~/.claude/settings.json` without taking anything over.
//!
//! The counterpart to [`crate::codex_config`], and it follows the same rule:
//! this is a file the user owns. `serde_json` is built with `preserve_order`,
//! so a round trip keeps their key order and every key we do not model — but
//! order is the easy half. The hard half is the status line itself, which is a
//! *single* slot: a user who has written their own has put work into it, and
//! replacing it to advertise ourselves would be the rudest thing in this
//! codebase.
//!
//! So: we fill the slot when it is empty, we leave it alone when it is not, and
//! we remove only what we put there.

use serde_json::{Map, Value};

/// Marks the entry as ours, so `disconnect` can remove exactly what `connect`
/// added and nothing else. A command string is not enough — the user may have
/// edited it, and a heuristic match on our binary's name would happily delete a
/// line somebody wrote themselves around `ironwire statusline`.
const OURS: &str = "ironwire";

/// What an edit would do, so the caller can show it before doing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Edit {
    /// The full file contents after the edit.
    pub contents: String,
    /// Human-readable lines describing what changed.
    pub changes: Vec<String>,
    /// Set when the user already has a status line of their own. Not an error:
    /// their line stays, and this says what they could add to it by hand.
    pub occupied_by: Option<String>,
}

impl Edit {
    pub(crate) fn is_noop(&self) -> bool {
        self.changes.is_empty()
    }
}

/// Add IronWire's status line, if the slot is free.
///
/// # Errors
///
/// Returns the parse error when the existing file is not valid JSON — we will
/// not rewrite a file we cannot read, because the user's own syntax error would
/// then look like ours.
pub(crate) fn connect(existing: &str, command: &str) -> Result<Edit, serde_json::Error> {
    let mut root = parse(existing)?;
    let mut changes = Vec::new();
    let mut occupied_by = None;

    match root.get("statusLine") {
        Some(current) if is_ours(current) => {
            // Ours already, but the path may have moved between installs.
            if current.get("command").and_then(Value::as_str) != Some(command) {
                root.insert("statusLine".to_string(), entry(command));
                changes.push(format!("statusLine: updated to `{command}`"));
            }
        }
        Some(current) => {
            occupied_by = Some(
                current
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("a status line of your own")
                    .to_string(),
            );
        }
        None => {
            root.insert("statusLine".to_string(), entry(command));
            changes.push(format!("statusLine: `{command}` (added)"));
        }
    }

    Ok(Edit {
        contents: render(&root),
        changes,
        occupied_by,
    })
}

/// Remove IronWire's status line, and only IronWire's.
///
/// # Errors
///
/// Returns the parse error when the existing file is not valid JSON.
pub(crate) fn disconnect(existing: &str) -> Result<Edit, serde_json::Error> {
    let mut root = parse(existing)?;
    let mut changes = Vec::new();
    if root.get("statusLine").is_some_and(is_ours) {
        root.remove("statusLine");
        changes.push("statusLine: removed".to_string());
    }
    Ok(Edit {
        contents: render(&root),
        changes,
        occupied_by: None,
    })
}

fn entry(command: &str) -> Value {
    serde_json::json!({
        "type": "command",
        "command": command,
        // Our own marker. Claude Code ignores keys it does not know, and this
        // is what makes removal exact rather than a guess.
        "installedBy": OURS,
    })
}

fn is_ours(value: &Value) -> bool {
    value.get("installedBy").and_then(Value::as_str) == Some(OURS)
}

/// An absent or blank file is an empty object, not a parse failure: "you have
/// no settings yet" is the commonest state, not an error.
fn parse(existing: &str) -> Result<Map<String, Value>, serde_json::Error> {
    if existing.trim().is_empty() {
        return Ok(Map::new());
    }
    serde_json::from_str(existing)
}

fn render(root: &Map<String, Value>) -> String {
    let mut out = serde_json::to_string_pretty(root).unwrap_or_else(|_| "{}".to_string());
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMAND: &str = "/usr/local/bin/ironwire statusline";

    #[test]
    fn an_empty_file_gains_a_status_line() {
        let edit = connect("", COMMAND).expect("valid");
        assert!(edit.contents.contains("statusLine"));
        assert!(edit.contents.contains(COMMAND));
        assert!(edit.occupied_by.is_none());
    }

    #[test]
    fn every_other_setting_survives_the_edit() {
        let existing = r#"{"model":"opus","permissions":{"allow":["Bash(ls:*)"]}}"#;
        let edit = connect(existing, COMMAND).expect("valid");
        let parsed: Value = serde_json::from_str(&edit.contents).expect("still JSON");
        assert_eq!(parsed["model"], "opus");
        assert_eq!(parsed["permissions"]["allow"][0], "Bash(ls:*)");
    }

    /// The rule this module exists for. Someone's own status line represents
    /// work, and it occupies the only slot there is.
    #[test]
    fn a_status_line_of_their_own_is_never_replaced() {
        let existing = r#"{"statusLine":{"type":"command","command":"~/bin/my-prompt.sh"}}"#;
        let edit = connect(existing, COMMAND).expect("valid");
        assert!(edit.is_noop(), "changed: {:?}", edit.changes);
        assert_eq!(edit.occupied_by.as_deref(), Some("~/bin/my-prompt.sh"));
        let parsed: Value = serde_json::from_str(&edit.contents).expect("still JSON");
        assert_eq!(parsed["statusLine"]["command"], "~/bin/my-prompt.sh");
    }

    #[test]
    fn installing_twice_changes_nothing_the_second_time() {
        let first = connect("", COMMAND).expect("valid");
        let second = connect(&first.contents, COMMAND).expect("valid");
        assert!(second.is_noop());
    }

    #[test]
    fn a_moved_binary_updates_the_command() {
        let first = connect("", "/old/path/ironwire statusline").expect("valid");
        let second = connect(&first.contents, COMMAND).expect("valid");
        assert!(!second.is_noop());
        assert!(second.contents.contains(COMMAND));
    }

    #[test]
    fn disconnect_removes_ours_and_leaves_the_rest() {
        let existing = connect(r#"{"model":"opus"}"#, COMMAND).expect("valid");
        let removed = disconnect(&existing.contents).expect("valid");
        let parsed: Value = serde_json::from_str(&removed.contents).expect("still JSON");
        assert!(parsed.get("statusLine").is_none());
        assert_eq!(parsed["model"], "opus");
    }

    /// A status line we did not install is not ours to remove, even on the way
    /// out.
    #[test]
    fn disconnect_leaves_a_status_line_we_did_not_install() {
        let existing = r#"{"statusLine":{"type":"command","command":"~/bin/mine.sh"}}"#;
        let removed = disconnect(existing).expect("valid");
        assert!(removed.is_noop());
        let parsed: Value = serde_json::from_str(&removed.contents).expect("still JSON");
        assert_eq!(parsed["statusLine"]["command"], "~/bin/mine.sh");
    }

    #[test]
    fn invalid_json_is_refused_rather_than_rewritten() {
        assert!(connect("{ not json", COMMAND).is_err());
        assert!(disconnect("{ not json").is_err());
    }
}
