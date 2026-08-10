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
    /// A format we do not have a safe editor for yet.
    UnsupportedFormat(ConfigFormat),
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
            Self::UnsupportedFormat(format) => write!(
                f,
                "IronWire cannot safely edit {format:?} for a catalog-described tool yet; \
                 set the key by hand"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Point a tool at IronWire.
///
/// # Errors
///
/// [`Error::Unparseable`] when the file is not valid, [`Error::Unusable`] when
/// the catalog entry fails validation, [`Error::UnsupportedFormat`] for a
/// format with no safe editor.
pub(crate) fn connect(agent: &AgentEntry, existing: &str, port: u16) -> Result<Edit, Error> {
    let format = usable(agent)?;
    match format {
        ConfigFormat::Json => json_connect(agent, existing, port),
        // Codex's config proves why: round-tripping TOML through a serializer
        // deletes the user's comments and ordering. A text editor for arbitrary
        // key paths is its own piece of work, and doing it badly is worse than
        // saying so.
        ConfigFormat::Toml => Err(Error::UnsupportedFormat(format)),
    }
}

/// Remove what [`connect`] added, and only that.
///
/// # Errors
///
/// As [`connect`].
pub(crate) fn disconnect(agent: &AgentEntry, existing: &str) -> Result<Edit, Error> {
    let format = usable(agent)?;
    match format {
        ConfigFormat::Json => json_disconnect(agent, existing),
        ConfigFormat::Toml => Err(Error::UnsupportedFormat(format)),
    }
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
    fn a_toml_tool_is_refused_rather_than_round_tripped() {
        let mut toml_agent = agent("model_providers.ironwire.base_url");
        toml_agent.config.file = "config.toml".to_string();
        let error = connect(&toml_agent, "", 8463).expect_err("refuses");
        assert!(matches!(
            error,
            Error::UnsupportedFormat(ConfigFormat::Toml)
        ));
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
