//! Editing `~/.codex/config.toml` without destroying it.
//!
//! Codex's config is a file the user owns and edits by hand. Round-tripping it
//! through a TOML serializer would silently delete their comments, their
//! ordering, and any key our struct does not model — which is a rude thing to do
//! to someone's config in exchange for setting two lines.
//!
//! So this module edits *text*, and parses only to check the result is still
//! valid TOML before anything is written.

/// The table IronWire owns. Nothing outside this block, and the one
/// `model_provider` line — and that one only when it is empty or already ours
/// — is ever touched.
const BLOCK_HEADER: &str = "[model_providers.ironwire]";

/// The one key outside our block that decides where Codex sends calls.
const MODEL_PROVIDER: &str = "model_provider";

/// Marker written by IronWire versions that replaced `model_provider` with our
/// own, so `ironwire disconnect codex` could put the old value back.
///
/// `connect` no longer writes one — a provider the user chose is left alone
/// now, so there is nothing to remember. It is still read, because configs
/// carrying a marker from an earlier version are on disk and disconnect owes
/// them their provider back.
const PREVIOUS_MARKER: &str = "# ironwire: previous model_provider =";

/// A slot that already held something the user put there.
///
/// The same shape, and the same meaning, as [`crate::claude_settings::Occupied`]
/// and the catalog's: their value stays, and the caller says what they could do
/// by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occupied {
    /// Which setting — `model_provider` is the only one this module can leave.
    pub slot: &'static str,
    /// What is in it, so the caller can name it back to them.
    pub current: String,
}

/// What an edit would do, so the caller can show it before doing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// The full file contents after the edit.
    pub contents: String,
    /// Human-readable lines describing what changed.
    pub changes: Vec<String>,
    /// Slots left alone because the user was already using them.
    pub occupied: Vec<Occupied>,
}

impl Edit {
    /// Whether this edit would change anything at all.
    pub fn is_noop(&self) -> bool {
        self.changes.is_empty()
    }

    /// What is in a slot we left alone, if we left that one alone.
    pub fn occupied_slot(&self, slot: &str) -> Option<&str> {
        self.occupied
            .iter()
            .find(|o| o.slot == slot)
            .map(|o| o.current.as_str())
    }
}

/// Produce the `[model_providers.ironwire]` block for a port.
fn block(port: u16) -> String {
    // `wire_api = "responses"` matters: Codex's Responses path is the one that
    // carries reasoning state, and downgrading it to chat completions would
    // quietly lose that. `env_key` is deliberately absent — IronWire supplies
    // the credential itself and strips whatever the client sends
    // (`docs/PROTOCOL.md` §2).
    format!(
        "{BLOCK_HEADER}\n\
         name = \"IronWire\"\n\
         base_url = \"http://127.0.0.1:{port}/openai/v1\"\n\
         wire_api = \"responses\"\n"
    )
}

/// Compute the edit that points Codex at IronWire.
///
/// Two slots, and only one of them is ever ours: `[model_providers.ironwire]`,
/// which nothing reads until it is selected, and the top-level
/// `model_provider`, which is what actually routes Codex. A `model_provider`
/// the user already chose is another proxy or a deliberate choice, so it is
/// reported and left — the same rule [`crate::claude_settings`] follows for
/// `ANTHROPIC_BASE_URL` and the catalog follows for every key it describes.
/// The block is still written either way, so selecting IronWire afterwards is
/// one word rather than four lines.
///
/// # Errors
///
/// Returns the parse error when the existing file is not valid TOML — we will
/// not append to a file we cannot read, because the user's own syntax error
/// would then look like ours.
pub fn connect(existing: &str, port: u16) -> Result<Edit, toml::de::Error> {
    existing.parse::<toml::Table>()?;

    let mut changes = Vec::new();
    let mut occupied = Vec::new();
    let mut out = replace_our_block(existing, &block(port), &mut changes);

    match top_level_model_provider(&out) {
        Some((_, value)) if value == "ironwire" => {}
        Some((_, value)) => occupied.push(Occupied {
            slot: MODEL_PROVIDER,
            current: value,
        }),
        None => {
            out = format!("{MODEL_PROVIDER} = \"ironwire\"\n{out}");
            changes.push(format!("{MODEL_PROVIDER} = \"ironwire\" (added)"));
        }
    }

    Ok(Edit {
        contents: out,
        changes,
        occupied,
    })
}

/// Compute the edit that undoes [`connect`].
///
/// `occupied` is always empty here, and that is a property rather than an
/// omission: an undo fills no slot, so there is no slot it can find full and
/// step around. What it does instead is remove only what IronWire put there —
/// a `model_provider` naming anyone else is left exactly as it is.
///
/// # Errors
///
/// Returns the parse error when the existing file is not valid TOML.
pub fn disconnect(existing: &str) -> Result<Edit, toml::de::Error> {
    existing.parse::<toml::Table>()?;

    // Whether the selection is ours to undo. A `model_provider` naming someone
    // else is one `connect` left alone, or one the user has chosen since;
    // either way, removing it would be this module deleting a line it never
    // wrote — and it would look to Codex exactly like a config with no provider
    // at all.
    let ours = top_level_model_provider(existing).is_some_and(|(_, value)| value == "ironwire");

    let mut changes = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut removing_block = false;
    let mut restore: Option<String> = None;

    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed == BLOCK_HEADER {
            removing_block = true;
            changes.push(format!("{BLOCK_HEADER}: removed"));
            continue;
        }
        if removing_block {
            // The block ends at the next table header, and only there — a blank
            // line inside a table is still inside the table.
            if trimmed.starts_with('[') {
                removing_block = false;
            } else {
                continue;
            }
        }
        if ours && let Some(previous) = trimmed.strip_prefix(PREVIOUS_MARKER) {
            restore = Some(previous.trim().trim_matches('"').to_string());
            continue;
        }
        lines.push(line.to_string());
    }

    // Put back whatever `model_provider` said before, or drop the line we added.
    // A marker is only ever honoured alongside our own selection, so a config
    // left by an older version — which did replace the provider — still gets it
    // back.
    let provider_line = lines
        .iter()
        .position(|l| is_top_level_assignment(l, MODEL_PROVIDER));
    if ours && let Some(index) = provider_line {
        match &restore {
            Some(previous) => {
                lines[index] = format!("{MODEL_PROVIDER} = \"{previous}\"");
                changes.push(format!("{MODEL_PROVIDER}: restored to \"{previous}\""));
            }
            None => {
                lines.remove(index);
                changes.push(format!("{MODEL_PROVIDER}: removed"));
            }
        }
    }

    Ok(Edit {
        contents: joined(&lines),
        changes,
        occupied: Vec::new(),
    })
}

/// Replace an existing IronWire block in place, or append one.
///
/// In place rather than remove-and-append so a user who moved our block
/// somewhere deliberate keeps it there.
fn replace_our_block(existing: &str, replacement: &str, changes: &mut Vec<String>) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut inside = false;
    let mut found = false;
    let mut original = Vec::new();

    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed == BLOCK_HEADER {
            inside = true;
            found = true;
            original.push(line.to_string());
            for replaced in replacement.lines() {
                lines.push(replaced.to_string());
            }
            continue;
        }
        if inside {
            if trimmed.starts_with('[') {
                inside = false;
            } else {
                original.push(line.to_string());
                continue;
            }
        }
        lines.push(line.to_string());
    }

    if found {
        let before = joined(&original);
        if before.trim() != replacement.trim() {
            changes.push(format!("{BLOCK_HEADER}: updated"));
        }
        return joined(&lines);
    }

    changes.push(format!("{BLOCK_HEADER}: added"));
    let mut out = joined(&lines);
    if !out.is_empty() && !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(replacement);
    out
}

/// Find a `model_provider = "..."` assignment at the top level.
///
/// Top level means before the first table header: `model_provider` inside
/// `[profiles.foo]` is a different setting entirely, and rewriting it would
/// change a profile the user did not ask us to touch.
fn top_level_model_provider(contents: &str) -> Option<(usize, String)> {
    for (index, line) in contents.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            return None;
        }
        if is_top_level_assignment(line, MODEL_PROVIDER) {
            let value = trimmed
                .split_once('=')?
                .1
                .trim()
                .trim_matches('"')
                .to_string();
            return Some((index, value));
        }
    }
    None
}

fn is_top_level_assignment(line: &str, key: &str) -> bool {
    let trimmed = line.trim();
    trimmed
        .strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

fn joined(lines: &[String]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Where Codex keeps its config. `CODEX_HOME` wins, as it does for Codex.
#[must_use]
pub fn path() -> Option<std::path::PathBuf> {
    if let Ok(home) = std::env::var("CODEX_HOME")
        && !home.is_empty()
    {
        return Some(std::path::PathBuf::from(home).join("config.toml"));
    }
    Some(dirs::home_dir()?.join(".codex").join("config.toml"))
}

/// Whether this config currently routes Codex through IronWire.
///
/// The provider block alone is not enough: it can sit there unselected. What
/// decides where traffic goes is the top-level `model_provider`.
#[must_use]
pub fn is_wired(existing: &str) -> bool {
    existing
        .parse::<toml::Table>()
        .ok()
        .and_then(|table| Some(table.get(MODEL_PROVIDER)?.as_str()? == "ironwire"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(contents: &str) -> toml::Table {
        contents.parse().expect("still valid TOML")
    }

    #[test]
    fn an_empty_config_gets_both_the_pointer_and_the_block() {
        let edit = connect("", 8463).expect("edits");
        let parsed = table(&edit.contents);
        assert_eq!(parsed["model_provider"].as_str(), Some("ironwire"));
        assert_eq!(
            parsed["model_providers"]["ironwire"]["base_url"].as_str(),
            Some("http://127.0.0.1:8463/openai/v1")
        );
        assert_eq!(
            parsed["model_providers"]["ironwire"]["wire_api"].as_str(),
            Some("responses")
        );
    }

    #[test]
    fn everything_the_user_wrote_survives() {
        // The reason this module edits text instead of round-tripping through a
        // serializer. A comment lost here is a comment lost forever.
        let existing = "\
# my notes about this file
model = \"gpt-5.6\"

[tui]
theme = \"dark\"  # trailing comment
";
        let edit = connect(existing, 8463).expect("edits");
        assert!(edit.contents.contains("# my notes about this file"));
        assert!(
            edit.contents
                .contains("theme = \"dark\"  # trailing comment")
        );
        assert_eq!(table(&edit.contents)["model"].as_str(), Some("gpt-5.6"));
    }

    #[test]
    fn a_provider_the_user_already_chose_is_reported_and_left() {
        // The rule `claude_settings` follows for `ANTHROPIC_BASE_URL`: a value
        // already there is another proxy or a deliberate choice, and taking it
        // over would move someone's traffic. The caller gets told instead.
        let existing = "model_provider = \"my-own-proxy\"\n";
        let edit = connect(existing, 8463).expect("edits");

        assert_eq!(
            table(&edit.contents)["model_provider"].as_str(),
            Some("my-own-proxy"),
            "IronWire took over a provider the user had chosen"
        );
        assert_eq!(edit.occupied_slot(MODEL_PROVIDER), Some("my-own-proxy"));
        assert!(!edit.contents.contains(PREVIOUS_MARKER));

        // The block is still written: unselected it routes nothing, and having
        // it there is what makes the manual fix one word.
        assert_eq!(
            table(&edit.contents)["model_providers"]["ironwire"]["base_url"].as_str(),
            Some("http://127.0.0.1:8463/openai/v1")
        );
        assert!(!is_wired(&edit.contents));
    }

    #[test]
    fn a_marker_left_by_an_older_version_still_restores_that_provider() {
        // Versions before the occupied-slot rule replaced `model_provider` and
        // remembered the old value in a comment. Those configs are on disk, and
        // disconnect owes them their provider back.
        let existing = format!(
            "{PREVIOUS_MARKER} \"openai\"\nmodel_provider = \"ironwire\"\n\n{}",
            block(8463)
        );
        let undone = disconnect(&existing).expect("edits");
        assert_eq!(
            table(&undone.contents)["model_provider"].as_str(),
            Some("openai"),
            "disconnect must put back what the user had"
        );
        assert!(!undone.contents.contains(BLOCK_HEADER));
        assert!(!undone.contents.contains(PREVIOUS_MARKER));
    }

    #[test]
    fn disconnect_leaves_a_provider_it_never_selected() {
        // Rule three: remove only what we put there. Dropping this line would
        // look to Codex like a config with no provider at all.
        let existing = "model_provider = \"my-own-proxy\"\n";
        let connected = connect(existing, 8463).expect("edits").contents;
        let undone = disconnect(&connected).expect("edits");
        assert_eq!(
            table(&undone.contents)["model_provider"].as_str(),
            Some("my-own-proxy")
        );
        assert_eq!(table(&undone.contents), table(existing));
    }

    #[test]
    fn an_undo_reports_no_occupied_slots_because_it_fills_none() {
        let connected = connect("", 8463).expect("edits").contents;
        assert!(disconnect(&connected).expect("edits").occupied.is_empty());
        assert!(
            disconnect("model_provider = \"my-own-proxy\"\n")
                .expect("edits")
                .occupied
                .is_empty()
        );
    }

    #[test]
    fn disconnecting_a_config_we_added_to_leaves_no_trace() {
        let original = "model = \"gpt-5.6\"\n\n[tui]\ntheme = \"dark\"\n";
        let connected = connect(original, 8463).expect("edits").contents;
        let restored = disconnect(&connected).expect("edits").contents;
        assert_eq!(table(&restored), table(original));
    }

    #[test]
    fn connecting_twice_changes_nothing_the_second_time() {
        let once = connect("", 8463).expect("edits").contents;
        let twice = connect(&once, 8463).expect("edits");
        assert_eq!(twice.contents, once);
        assert!(twice.is_noop(), "reported {:?}", twice.changes);
    }

    #[test]
    fn a_port_change_rewrites_the_block_in_place_rather_than_appending_a_second_one() {
        let once = connect("", 8463).expect("edits").contents;
        let moved = connect(&once, 9000).expect("edits");
        assert_eq!(
            moved.contents.matches(BLOCK_HEADER).count(),
            1,
            "a second block would leave Codex reading a stale one"
        );
        assert!(moved.contents.contains("127.0.0.1:9000"));
        assert!(!moved.is_noop());
    }

    #[test]
    fn a_block_the_user_moved_stays_where_they_put_it() {
        let existing = format!(
            "model = \"gpt-5.6\"\n\n{}\n\n[tui]\ntheme = \"dark\"\n",
            block(8463).trim_end()
        );
        let edit = connect(&existing, 9000).expect("edits");
        let block_at = edit.contents.find(BLOCK_HEADER).expect("block present");
        let tui_at = edit.contents.find("[tui]").expect("tui present");
        assert!(block_at < tui_at, "our block jumped past the user's table");
    }

    #[test]
    fn a_model_provider_inside_a_profile_is_left_alone() {
        // `[profiles.work] model_provider` is a different setting; rewriting it
        // would change a profile the user did not ask us to touch.
        let existing = "[profiles.work]\nmodel_provider = \"openai\"\n";
        let edit = connect(existing, 8463).expect("edits");
        let parsed = table(&edit.contents);
        assert_eq!(
            parsed["profiles"]["work"]["model_provider"].as_str(),
            Some("openai")
        );
        assert_eq!(parsed["model_provider"].as_str(), Some("ironwire"));
    }

    #[test]
    fn a_config_we_cannot_parse_is_refused_rather_than_appended_to() {
        // Appending to a file with a syntax error makes the user's bug look
        // like ours, and they will delete our block trying to fix it.
        assert!(connect("this is not = = toml\n", 8463).is_err());
        assert!(disconnect("this is not = = toml\n").is_err());
    }
}
