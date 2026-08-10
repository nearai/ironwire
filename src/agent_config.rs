//! Pointing a catalog-described tool at IronWire.
//!
//! [`crate::claude_settings`] and [`crate::codex_config`] handle the two agents
//! this binary ships knowing about, and they do more than set a URL — a status
//! line, a provider table, a warning about what Codex cannot change afterwards.
//! This module is for the rest: tools the signed catalog introduces, where all
//! we know is a config file and a key to set in it
//! (`ironwire_catalog::schema::AgentEntry`).
//!
//! Every rule the hand-written two follow applies here, because they are what
//! makes editing someone else's config acceptable at all:
//!
//! - **Never rewrite a file we cannot parse.** A user's own syntax error must
//!   not come back looking like ours.
//! - **Fill an empty slot; leave a full one alone.** A value already in the key
//!   is another proxy or a deliberate choice, and taking it over would move
//!   someone's traffic without telling them. It is reported, not overwritten.
//! - **Remove only what we put there.**
//!
//! The value written is never the catalog's to choose: it comes from
//! [`Facade::url`], which is loopback and this daemon's port.

use std::path::Path;

use ironwire_catalog::schema::{AgentEntry, ConfigFormat, Facade};
use serde_json::{Map, Value};

/// A slot that already held something the user put there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Occupied {
    /// The key path, so the caller can name it back to them.
    pub slot: String,
    /// What is in it.
    pub current: String,
}

/// What an edit would do, so the caller can show it before doing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Edit {
    /// The full file contents after the edit.
    pub contents: String,
    /// Human-readable lines describing what changed.
    pub changes: Vec<String>,
    /// Slots left alone because the user was already using them.
    pub occupied: Vec<Occupied>,
}

impl Edit {
    pub(crate) fn is_noop(&self) -> bool {
        self.changes.is_empty()
    }
}

/// Why an edit could not be produced.
#[derive(Debug)]
pub(crate) enum Error {
    /// The existing file is not valid for its format.
    Unparseable(String),
    /// The entry did not survive validation, so we will not act on it.
    Unusable(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unparseable(detail) => write!(
                f,
                "the file is not valid for its format — IronWire will not \
                 rewrite a file it cannot read: {detail}"
            ),
            Self::Unusable(detail) => write!(f, "the catalog entry is not usable: {detail}"),
        }
    }
}

impl std::error::Error for Error {}

/// Point a tool at IronWire.
///
/// # Errors
///
/// [`Error::Unparseable`] when the file is not valid, [`Error::Unusable`] when
/// the catalog entry fails validation. A shape this module will not edit by
/// hand is reported as an occupied slot rather than raised as an error.
pub(crate) fn connect(agent: &AgentEntry, existing: &str, port: u16) -> Result<Edit, Error> {
    match usable(agent)? {
        ConfigFormat::Json => json_connect(agent, existing, port),
        ConfigFormat::Toml => toml_connect(agent, existing, port),
    }
}

/// Remove what [`connect`] added, and only that.
///
/// # Errors
///
/// As [`connect`].
pub(crate) fn disconnect(agent: &AgentEntry, existing: &str) -> Result<Edit, Error> {
    match usable(agent)? {
        ConfigFormat::Json => json_disconnect(agent, existing),
        ConfigFormat::Toml => toml_disconnect(agent, existing),
    }
}

/// Whether this tool looks installed on the machine.
///
/// Mirrors `claude_installed` / `codex_installed`, and for the same reason both
/// halves are needed: a tool installed but never run has no config directory
/// yet, and one installed outside `PATH` — an app bundle, a version-manager
/// shim — still leaves its directory behind. Either is enough.
///
/// `on_path` is injected so this is decidable without a real `PATH`, and so the
/// rule can be tested rather than only observed.
pub(crate) fn detected(agent: &AgentEntry, home: &Path, on_path: &dyn Fn(&str) -> bool) -> bool {
    // An entry we would refuse to act on is not one to report as present:
    // offering to wire a tool and then failing at the file is worse than never
    // offering.
    if agent.problem().is_some() {
        return false;
    }
    if agent
        .config
        .resolve(home)
        .parent()
        .is_some_and(Path::exists)
    {
        return true;
    }
    agent.detect.iter().any(|name| on_path(name))
}

/// The entry's format, once it has passed the schema's own validation.
///
/// Checked here as well as at load, because an entry reaching a file writer is
/// the last place it is cheap to refuse.
fn usable(agent: &AgentEntry) -> Result<ConfigFormat, Error> {
    if let Some(problem) = agent.problem() {
        return Err(Error::Unusable(problem));
    }
    agent
        .config
        .format()
        .ok_or_else(|| Error::Unusable("no format".to_string()))
}

// ---------------------------------------------------------------------- JSON

fn json_connect(agent: &AgentEntry, existing: &str, port: u16) -> Result<Edit, Error> {
    let mut root = json_parse(existing)?;
    let mut changes = Vec::new();
    let mut occupied = Vec::new();

    for setting in &agent.settings {
        let url = setting.facade.url(port);
        let path: Vec<&str> = setting.key.split('.').collect();
        match json_slot(&root, &path) {
            Slot::Blocked(current) => occupied.push(Occupied {
                slot: setting.key.clone(),
                current,
            }),
            Slot::Ours(current) => {
                if current != url {
                    json_insert(&mut root, &path, &url);
                    changes.push(format!("{}: updated to `{url}`", setting.key));
                }
            }
            Slot::Empty => {
                json_insert(&mut root, &path, &url);
                changes.push(format!("{}: `{url}` (added)", setting.key));
            }
        }
    }

    Ok(Edit {
        contents: json_render(&root),
        changes,
        occupied,
    })
}

fn json_disconnect(agent: &AgentEntry, existing: &str) -> Result<Edit, Error> {
    let mut root = json_parse(existing)?;
    let mut changes = Vec::new();

    for setting in &agent.settings {
        let path: Vec<&str> = setting.key.split('.').collect();
        if let Slot::Ours(_) = json_slot(&root, &path) {
            json_remove(&mut root, &path);
            changes.push(format!("{}: removed", setting.key));
        }
    }

    Ok(Edit {
        contents: json_render(&root),
        changes,
        occupied: Vec::new(),
    })
}

/// What is in a key path, from this module's point of view.
enum Slot {
    /// Nothing there, or a parent that does not exist yet.
    Empty,
    /// A value we wrote — loopback, our own façade path.
    Ours(String),
    /// Something else, with a description of it. Never overwritten.
    Blocked(String),
}

fn json_slot(root: &Map<String, Value>, path: &[&str]) -> Slot {
    let Some((leaf, parents)) = path.split_last() else {
        return Slot::Empty;
    };

    let mut current = root;
    for segment in parents {
        match current.get(*segment) {
            Some(Value::Object(next)) => current = next,
            // A parent that exists but is not an object is not ours to fix, and
            // saying so is more honest than silently replacing it.
            Some(_) => return Slot::Blocked(format!("`{segment}` is not an object")),
            None => return Slot::Empty,
        }
    }

    match current.get(*leaf) {
        None => Slot::Empty,
        Some(Value::String(value)) if points_at_us(value) => Slot::Ours(value.clone()),
        Some(Value::String(value)) => Slot::Blocked(value.clone()),
        Some(other) => Slot::Blocked(other.to_string()),
    }
}

/// Whether a URL is one of ours.
///
/// Loopback and one of our own façade paths. A port change still counts as
/// ours, which is what lets `connect` follow the daemon to a new port instead
/// of treating its own previous value as somebody else's.
fn points_at_us(url: &str) -> bool {
    let rest = url
        .strip_prefix("http://127.0.0.1:")
        .or_else(|| url.strip_prefix("http://localhost:"));
    rest.is_some_and(|rest| {
        let trimmed = rest.trim_end_matches('/');
        trimmed.split_once('/').is_some_and(|(port, path)| {
            port.chars().all(|c| c.is_ascii_digit())
                && Facade::ALL
                    .iter()
                    .any(|facade| facade.path() == format!("/{path}"))
        })
    })
}

fn json_insert(root: &mut Map<String, Value>, path: &[&str], value: &str) {
    let Some((leaf, parents)) = path.split_last() else {
        return;
    };
    let mut current = root;
    for segment in parents {
        current = current
            .entry((*segment).to_string())
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            // `json_slot` reported a non-object parent as blocked, so this
            // branch is unreachable for any path we chose to write.
            .expect("parent is an object");
    }
    current.insert((*leaf).to_string(), Value::String(value.to_string()));
}

fn json_remove(root: &mut Map<String, Value>, path: &[&str]) {
    let Some((leaf, parents)) = path.split_last() else {
        return;
    };
    // Reborrowed, not moved: `prune_empty` needs the root again below.
    let mut current = &mut *root;
    for segment in parents {
        match current.get_mut(*segment).and_then(Value::as_object_mut) {
            Some(next) => current = next,
            None => return,
        }
    }
    current.remove(*leaf);

    // A container we emptied is one we created. Anything the user put beside
    // our key keeps it alive, which is the behaviour `claude_settings` has for
    // `env` and the reason it is worth doing here too.
    prune_empty(root, parents);
}

/// Drop containers that our own removal just emptied, deepest first.
///
/// Iterative, re-descending from the root each round: a recursive version
/// cannot hand out a `&mut` to a nested map and then mutate an ancestor.
fn prune_empty(root: &mut Map<String, Value>, parents: &[&str]) {
    for depth in (1..=parents.len()).rev() {
        let Some((last, above)) = parents[..depth].split_last() else {
            return;
        };
        let mut current = &mut *root;
        for segment in above {
            match current.get_mut(*segment).and_then(Value::as_object_mut) {
                Some(next) => current = next,
                None => return,
            }
        }
        let empty = current
            .get(*last)
            .and_then(Value::as_object)
            .is_some_and(Map::is_empty);
        if !empty {
            return;
        }
        current.remove(*last);
    }
}

fn json_parse(existing: &str) -> Result<Map<String, Value>, Error> {
    if existing.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(existing) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(Error::Unparseable(
            "the file is valid JSON but not an object".to_string(),
        )),
        Err(error) => Err(Error::Unparseable(error.to_string())),
    }
}

fn json_render(root: &Map<String, Value>) -> String {
    let mut out = serde_json::to_string_pretty(root).unwrap_or_else(|_| "{}".to_string());
    out.push('\n');
    out
}

// ---------------------------------------------------------------------- TOML
//
// Edited as *text*, for the reason `codex_config` gives: round-tripping a TOML
// file through a serializer deletes the user's comments, their ordering, and
// anything our types do not model. Parsing happens twice — once to refuse a
// file we cannot read, once on the result to refuse an edit we cannot stand
// behind — and the value is read back out of that second parse to prove it
// landed where we meant it to.
//
// The shapes this handles are the ones a tool's config actually uses: a
// top-level key, and a key inside a `[table]` or `[table.sub]` header. Anything
// else — a table written inline, a key already spelled as a dotted path, an
// array of tables — is reported rather than guessed at. Refusing is a worse
// user experience than succeeding and a much better one than corrupting a file.

fn toml_connect(agent: &AgentEntry, existing: &str, port: u16) -> Result<Edit, Error> {
    toml_parse(existing)?;

    let mut out = existing.to_string();
    let mut changes = Vec::new();
    let mut occupied = Vec::new();
    let mut written: Vec<(String, String)> = Vec::new();

    for setting in &agent.settings {
        let url = setting.facade.url(port);
        let table = toml_parse(&out)?;
        match toml_slot(&table, &setting.key) {
            Slot::Blocked(current) => {
                occupied.push(Occupied {
                    slot: setting.key.clone(),
                    current,
                });
                continue;
            }
            Slot::Ours(current) if current == url => continue,
            Slot::Ours(_) => match toml_replace(&out, &setting.key, &url) {
                Some(next) => {
                    out = next;
                    changes.push(format!("{}: updated to `{url}`", setting.key));
                    written.push((setting.key.clone(), url));
                }
                None => occupied.push(Occupied {
                    slot: setting.key.clone(),
                    current: "a shape IronWire will not edit by hand".to_string(),
                }),
            },
            Slot::Empty => match toml_insert(&out, &setting.key, &url) {
                Some(next) => {
                    out = next;
                    changes.push(format!("{}: `{url}` (added)", setting.key));
                    written.push((setting.key.clone(), url));
                }
                None => occupied.push(Occupied {
                    slot: setting.key.clone(),
                    current: "a table IronWire cannot safely extend".to_string(),
                }),
            },
        }
    }

    verify_written(&out, &written)?;
    Ok(Edit {
        contents: out,
        changes,
        occupied,
    })
}

fn toml_disconnect(agent: &AgentEntry, existing: &str) -> Result<Edit, Error> {
    toml_parse(existing)?;

    let mut out = existing.to_string();
    let mut changes = Vec::new();

    for setting in &agent.settings {
        let table = toml_parse(&out)?;
        if let Slot::Ours(_) = toml_slot(&table, &setting.key)
            && let Some(next) = toml_remove_line(&out, &setting.key)
        {
            out = next;
            changes.push(format!("{}: removed", setting.key));
        }
    }

    toml_parse(&out)?;
    Ok(Edit {
        contents: out,
        changes,
        occupied: Vec::new(),
    })
}

/// The result must still parse, and every key we *wrote* must read back as the
/// value we wrote.
///
/// The read-back is the half that matters: a textual edit which produced valid
/// TOML with the value in the wrong table would otherwise look like a success.
/// Only written keys are checked — a slot we deliberately left alone is
/// supposed to be unchanged, and asserting on it would fail the very refusals
/// this module exists to make.
fn verify_written(contents: &str, written: &[(String, String)]) -> Result<(), Error> {
    let table = toml_parse(contents)?;
    for (key, url) in written {
        match toml_slot(&table, key) {
            Slot::Ours(current) if &current == url => {}
            _ => {
                return Err(Error::Unparseable(format!(
                    "after editing, `{key}` is not the value IronWire wrote — refusing to save"
                )));
            }
        }
    }
    Ok(())
}

fn toml_parse(contents: &str) -> Result<toml::Table, Error> {
    contents
        .parse::<toml::Table>()
        .map_err(|error| Error::Unparseable(error.to_string()))
}

fn toml_slot(table: &toml::Table, key: &str) -> Slot {
    let mut path = key.split('.').peekable();
    let mut current = table;
    while let Some(segment) = path.next() {
        if path.peek().is_none() {
            return match current.get(segment) {
                None => Slot::Empty,
                Some(toml::Value::String(value)) if points_at_us(value) => {
                    Slot::Ours(value.clone())
                }
                Some(toml::Value::String(value)) => Slot::Blocked(value.clone()),
                Some(other) => Slot::Blocked(other.to_string()),
            };
        }
        match current.get(segment) {
            Some(toml::Value::Table(next)) => current = next,
            Some(_) => return Slot::Blocked(format!("`{segment}` is not a table")),
            None => return Slot::Empty,
        }
    }
    Slot::Empty
}

/// The `[header]` line for a table path, if the file spells it that way.
///
/// `None` covers both "not there" and "there, but written inline or as a dotted
/// key" — neither of which this module will edit.
fn toml_header_line(contents: &str, table_path: &[&str]) -> Option<usize> {
    let wanted = format!("[{}]", table_path.join("."));
    contents.lines().position(|line| line.trim() == wanted)
}

/// Where a table's block ends: the next header, or end of file.
fn toml_block_end(lines: &[&str], header: usize) -> usize {
    lines
        .iter()
        .enumerate()
        .skip(header + 1)
        .find(|(_, line)| line.trim_start().starts_with('['))
        .map_or(lines.len(), |(index, _)| index)
}

/// The line index of `key = ...` within a range, ignoring comments.
fn toml_key_line(lines: &[&str], range: std::ops::Range<usize>, key: &str) -> Option<usize> {
    lines[range.clone()]
        .iter()
        .position(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with('#')
                && trimmed
                    .strip_prefix(key)
                    .is_some_and(|rest| rest.trim_start().starts_with('='))
        })
        .map(|offset| range.start + offset)
}

fn toml_insert(contents: &str, key: &str, value: &str) -> Option<String> {
    let mut segments: Vec<&str> = key.split('.').collect();
    let leaf = segments.pop()?;
    let assignment = format!("{leaf} = \"{value}\"");
    let mut lines: Vec<String> = contents.lines().map(str::to_string).collect();

    if segments.is_empty() {
        // A top-level key has to come before the first table header, or TOML
        // reads it as belonging to that table.
        let at = lines
            .iter()
            .position(|line| line.trim_start().starts_with('['))
            .unwrap_or(lines.len());
        lines.insert(at, assignment);
        return Some(joined(&lines));
    }

    match toml_header_line(contents, &segments) {
        Some(header) => {
            lines.insert(header + 1, assignment);
            Some(joined(&lines))
        }
        None => {
            // No header for it. Only safe to add one when nothing of that path
            // exists yet — otherwise it is spelled inline or dotted somewhere,
            // and a second definition is a parse error at best.
            let table = contents.parse::<toml::Table>().ok()?;
            if !matches!(toml_slot(&table, key), Slot::Empty) {
                return None;
            }
            let mut walked = &table;
            for segment in &segments {
                match walked.get(*segment) {
                    Some(toml::Value::Table(next)) => walked = next,
                    Some(_) => return None,
                    None => {
                        // Nothing of this path exists: a fresh header is safe.
                        let mut out = joined(&lines);
                        if !out.is_empty() && !out.ends_with('\n') {
                            out.push('\n');
                        }
                        out.push_str(&format!("\n[{}]\n{assignment}\n", segments.join(".")));
                        return Some(out);
                    }
                }
            }
            // The table exists but has no header line, so it is inline.
            None
        }
    }
}

fn toml_replace(contents: &str, key: &str, value: &str) -> Option<String> {
    let mut segments: Vec<&str> = key.split('.').collect();
    let leaf = segments.pop()?;
    let mut lines: Vec<String> = contents.lines().map(str::to_string).collect();
    let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();

    let range = if segments.is_empty() {
        0..borrowed
            .iter()
            .position(|line| line.trim_start().starts_with('['))
            .unwrap_or(borrowed.len())
    } else {
        let header = toml_header_line(contents, &segments)?;
        (header + 1)..toml_block_end(&borrowed, header)
    };

    let at = toml_key_line(&borrowed, range, leaf)?;
    let indent: String = lines[at]
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();
    lines[at] = format!("{indent}{leaf} = \"{value}\"");
    Some(joined(&lines))
}

fn toml_remove_line(contents: &str, key: &str) -> Option<String> {
    let mut segments: Vec<&str> = key.split('.').collect();
    let leaf = segments.pop()?;
    let mut lines: Vec<String> = contents.lines().map(str::to_string).collect();
    let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();

    let (range, header) = if segments.is_empty() {
        (
            0..borrowed
                .iter()
                .position(|line| line.trim_start().starts_with('['))
                .unwrap_or(borrowed.len()),
            None,
        )
    } else {
        let header = toml_header_line(contents, &segments)?;
        (
            (header + 1)..toml_block_end(&borrowed, header),
            Some(header),
        )
    };

    let at = toml_key_line(&borrowed, range.clone(), leaf)?;
    lines.remove(at);

    // A header whose block we just emptied is one we added. Anything the user
    // put in it keeps it, same as the JSON side.
    if let Some(header) = header {
        let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
        let end = toml_block_end(&borrowed, header);
        let empty = borrowed[(header + 1)..end]
            .iter()
            .all(|line| line.trim().is_empty());
        if empty {
            lines.drain(header..end);
        }
    }
    Some(joined(&lines))
}

fn joined(lines: &[String]) -> String {
    let mut out = lines.join("\n");
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironwire_catalog::schema::{AgentSetting, ConfigLocation};

    fn agent(key: &str) -> AgentEntry {
        AgentEntry {
            id: "tool".to_string(),
            name: "A Tool".to_string(),
            enabled: true,
            detect: vec!["tool".to_string()],
            config: ConfigLocation {
                dir: vec![".tool".to_string()],
                file: "config.json".to_string(),
            },
            settings: vec![AgentSetting {
                key: key.to_string(),
                facade: Facade::Anthropic,
            }],
        }
    }

    #[test]
    fn an_empty_slot_is_filled_with_our_own_loopback_url() {
        let edit = connect(&agent("env.ANTHROPIC_BASE_URL"), "{}", 8463).expect("edits");
        assert!(edit.contents.contains("http://127.0.0.1:8463/anthropic"));
        assert!(edit.occupied.is_empty());
        assert_eq!(edit.changes.len(), 1);
    }

    /// The rule that makes editing someone else's config acceptable: a value
    /// they put there stays, and they are told rather than surprised.
    #[test]
    fn a_slot_someone_else_is_using_is_reported_and_left_alone() {
        let existing = r#"{"env":{"ANTHROPIC_BASE_URL":"https://my-own-proxy.example"}}"#;
        let edit = connect(&agent("env.ANTHROPIC_BASE_URL"), existing, 8463).expect("edits");
        assert!(edit.is_noop(), "{:?}", edit.changes);
        assert_eq!(edit.occupied.len(), 1);
        assert_eq!(edit.occupied[0].current, "https://my-own-proxy.example");
        assert!(edit.contents.contains("my-own-proxy.example"));
    }

    /// A port change is still ours to follow — otherwise the daemon moving
    /// would look like somebody else's value and never be corrected.
    #[test]
    fn our_own_value_is_followed_to_a_new_port() {
        let existing = r#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:9999/anthropic"}}"#;
        let edit = connect(&agent("env.ANTHROPIC_BASE_URL"), existing, 8463).expect("edits");
        assert_eq!(edit.changes.len(), 1);
        assert!(edit.contents.contains("127.0.0.1:8463/anthropic"));
        assert!(edit.occupied.is_empty());
    }

    #[test]
    fn everything_the_user_wrote_survives_the_edit() {
        let existing = r#"{"theme":"dark","env":{"MY_KEY":"mine"}}"#;
        let edit = connect(&agent("env.ANTHROPIC_BASE_URL"), existing, 8463).expect("edits");
        assert!(edit.contents.contains("\"theme\": \"dark\""));
        assert!(edit.contents.contains("\"MY_KEY\": \"mine\""));
    }

    #[test]
    fn a_file_we_cannot_read_is_never_rewritten() {
        let error = connect(&agent("env.X"), "{ not json", 8463).expect_err("refuses");
        assert!(matches!(error, Error::Unparseable(_)));
    }

    #[test]
    fn a_parent_that_is_not_an_object_is_reported_rather_than_replaced() {
        let existing = r#"{"env":"a string the user put here"}"#;
        let edit = connect(&agent("env.ANTHROPIC_BASE_URL"), existing, 8463).expect("edits");
        assert!(edit.is_noop());
        assert_eq!(edit.occupied.len(), 1);
        assert!(edit.contents.contains("a string the user put here"));
    }

    #[test]
    fn disconnect_removes_ours_and_nothing_else() {
        let existing =
            r#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:8463/anthropic","MY_KEY":"mine"}}"#;
        let edit = disconnect(&agent("env.ANTHROPIC_BASE_URL"), existing).expect("edits");
        assert_eq!(edit.changes.len(), 1);
        assert!(!edit.contents.contains("ANTHROPIC_BASE_URL"));
        assert!(edit.contents.contains("MY_KEY"));
    }

    #[test]
    fn disconnect_leaves_a_value_that_was_never_ours() {
        let existing = r#"{"env":{"ANTHROPIC_BASE_URL":"https://my-own-proxy.example"}}"#;
        let edit = disconnect(&agent("env.ANTHROPIC_BASE_URL"), existing).expect("edits");
        assert!(edit.is_noop());
        assert!(edit.contents.contains("my-own-proxy.example"));
    }

    /// A container that only ever held our key goes with it; one the user was
    /// also using stays.
    #[test]
    fn a_container_we_emptied_is_removed_and_a_shared_one_is_kept() {
        let ours_only = r#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:8463/anthropic"}}"#;
        let edit = disconnect(&agent("env.ANTHROPIC_BASE_URL"), ours_only).expect("edits");
        assert!(!edit.contents.contains("env"), "{}", edit.contents);

        let shared =
            r#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:8463/anthropic","MY_KEY":"x"}}"#;
        let edit = disconnect(&agent("env.ANTHROPIC_BASE_URL"), shared).expect("edits");
        assert!(edit.contents.contains("env"));
    }

    #[test]
    fn a_toml_table_is_created_when_nothing_of_it_exists() {
        let edit = connect(&toml_agent("providers.ironwire.base_url"), "", 8463).expect("edits");
        assert!(edit.contents.contains("[providers.ironwire]"));
        assert!(
            edit.contents
                .contains("base_url = \"http://127.0.0.1:8463/anthropic\"")
        );
        assert!(edit.contents.parse::<toml::Table>().is_ok());
    }

    fn toml_agent(key: &str) -> AgentEntry {
        let mut entry = agent(key);
        entry.config.file = "config.toml".to_string();
        entry
    }

    #[test]
    fn a_key_goes_into_a_table_that_already_has_a_header() {
        let existing = "# mine\n[providers.ironwire]\nwire_api = \"responses\"\n";
        let edit = connect(&toml_agent("providers.ironwire.base_url"), existing, 8463).expect("e");
        assert!(edit.contents.contains("# mine"), "{}", edit.contents);
        assert!(edit.contents.contains("wire_api = \"responses\""));
        let parsed: toml::Table = edit.contents.parse().expect("valid");
        assert_eq!(
            parsed["providers"]["ironwire"]["base_url"].as_str(),
            Some("http://127.0.0.1:8463/anthropic")
        );
    }

    /// The whole reason this is a text edit and not a round trip.
    #[test]
    fn comments_and_ordering_survive() {
        let existing = "# a comment the user wrote\nmodel = \"x\"\n\n[other]\nkeep = 1\n";
        let edit = connect(&toml_agent("providers.p.base_url"), existing, 8463).expect("edits");
        assert!(edit.contents.contains("# a comment the user wrote"));
        assert!(edit.contents.contains("model = \"x\""));
        assert!(edit.contents.contains("[other]"));
        assert!(edit.contents.contains("keep = 1"));
    }

    #[test]
    fn a_toml_slot_someone_else_is_using_is_left_alone() {
        let existing = "[providers.ironwire]\nbase_url = \"https://their-proxy.example\"\n";
        let edit = connect(&toml_agent("providers.ironwire.base_url"), existing, 8463).expect("e");
        assert!(edit.is_noop());
        assert_eq!(edit.occupied.len(), 1);
        assert!(edit.contents.contains("their-proxy.example"));
    }

    #[test]
    fn our_own_toml_value_is_followed_to_a_new_port() {
        let existing = "[providers.ironwire]\nbase_url = \"http://127.0.0.1:1111/anthropic\"\n";
        let edit = connect(&toml_agent("providers.ironwire.base_url"), existing, 8463).expect("e");
        assert_eq!(edit.changes.len(), 1);
        let parsed: toml::Table = edit.contents.parse().expect("valid");
        assert_eq!(
            parsed["providers"]["ironwire"]["base_url"].as_str(),
            Some("http://127.0.0.1:8463/anthropic")
        );
    }

    /// A table written inline cannot be extended by adding a header — that
    /// would be a second definition and a parse error. Reported, not attempted.
    #[test]
    fn an_inline_table_is_reported_rather_than_given_a_second_definition() {
        let existing = "providers = { ironwire = { wire_api = \"responses\" } }\n";
        let edit = connect(&toml_agent("providers.ironwire.base_url"), existing, 8463).expect("e");
        assert!(edit.is_noop(), "{:?}", edit.changes);
        assert_eq!(edit.occupied.len(), 1);
        assert_eq!(edit.contents, existing);
    }

    #[test]
    fn a_top_level_key_lands_before_the_first_table_header() {
        let existing = "[other]\nkeep = 1\n";
        let edit = connect(&toml_agent("base_url"), existing, 8463).expect("edits");
        let parsed: toml::Table = edit.contents.parse().expect("valid");
        // If it landed after the header it would belong to `other`.
        assert!(parsed.get("base_url").is_some(), "{}", edit.contents);
        assert_eq!(parsed["other"]["keep"].as_integer(), Some(1));
    }

    #[test]
    fn a_toml_file_we_cannot_read_is_never_rewritten() {
        let error = connect(&toml_agent("a.b"), "not = = toml", 8463).expect_err("refuses");
        assert!(matches!(error, Error::Unparseable(_)));
    }

    #[test]
    fn toml_disconnect_removes_ours_and_the_header_it_emptied() {
        let existing = "[providers.ironwire]\nbase_url = \"http://127.0.0.1:8463/anthropic\"\n";
        let edit = disconnect(&toml_agent("providers.ironwire.base_url"), existing).expect("e");
        assert_eq!(edit.changes.len(), 1);
        assert!(!edit.contents.contains("base_url"));
        assert!(
            !edit.contents.contains("[providers.ironwire]"),
            "{}",
            edit.contents
        );
    }

    #[test]
    fn toml_disconnect_keeps_a_table_the_user_is_also_using() {
        let existing = "[providers.ironwire]\nbase_url = \"http://127.0.0.1:8463/anthropic\"\nwire_api = \"responses\"\n";
        let edit = disconnect(&toml_agent("providers.ironwire.base_url"), existing).expect("e");
        assert!(edit.contents.contains("[providers.ironwire]"));
        assert!(edit.contents.contains("wire_api"));
        assert!(!edit.contents.contains("base_url"));
    }

    #[test]
    fn toml_disconnect_leaves_a_value_that_was_never_ours() {
        let existing = "[providers.ironwire]\nbase_url = \"https://their-proxy.example\"\n";
        let edit = disconnect(&toml_agent("providers.ironwire.base_url"), existing).expect("e");
        assert!(edit.is_noop());
        assert!(edit.contents.contains("their-proxy.example"));
    }

    // ------------------------------------------------------------- detection

    #[test]
    fn a_tool_with_a_config_directory_is_detected_even_when_it_is_not_on_path() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".tool")).expect("mkdir");
        // An app bundle or a version-manager shim leaves no name on PATH.
        assert!(detected(
            &agent("env.ANTHROPIC_BASE_URL"),
            home.path(),
            &|_| false
        ));
    }

    #[test]
    fn a_tool_on_path_is_detected_before_it_has_ever_been_run() {
        let home = tempfile::tempdir().expect("tempdir");
        // No config directory yet — it has not been started.
        assert!(detected(
            &agent("env.ANTHROPIC_BASE_URL"),
            home.path(),
            &|name| name == "tool"
        ));
    }

    #[test]
    fn a_tool_that_is_neither_is_not_detected() {
        let home = tempfile::tempdir().expect("tempdir");
        assert!(!detected(
            &agent("env.ANTHROPIC_BASE_URL"),
            home.path(),
            &|_| false
        ));
    }

    /// Offering to wire a tool and then refusing at the file is worse than
    /// never offering, so an entry we would not act on is not "present".
    #[test]
    fn an_entry_we_would_refuse_to_write_is_never_reported_as_installed() {
        let home = tempfile::tempdir().expect("tempdir");
        let mut escaping = agent("env.ANTHROPIC_BASE_URL");
        escaping.config.dir = vec!["..".to_string()];
        assert!(!detected(&escaping, home.path(), &|_| true));
    }

    /// A catalog entry that failed validation must not reach a file at all.
    #[test]
    fn an_unusable_entry_never_reaches_the_filesystem() {
        let mut escaping = agent("env.X");
        escaping.config.dir = vec!["..".to_string()];
        let error = connect(&escaping, "{}", 8463).expect_err("refuses");
        assert!(matches!(error, Error::Unusable(_)));
    }

    #[test]
    fn an_absent_file_starts_from_an_empty_object() {
        let edit = connect(&agent("env.ANTHROPIC_BASE_URL"), "", 8463).expect("edits");
        assert!(edit.contents.contains("127.0.0.1:8463/anthropic"));
    }
}
