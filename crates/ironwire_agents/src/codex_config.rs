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
/// `model_provider` line, is ever touched.
const BLOCK_HEADER: &str = "[model_providers.ironwire]";

/// Marker used to remember what `model_provider` said before we changed it, so
/// `ironwire disconnect codex` can put it back.
const PREVIOUS_MARKER: &str = "# ironwire: previous model_provider =";

/// What an edit would do, so the caller can show it before doing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// The full file contents after the edit.
    pub contents: String,
    /// Human-readable lines describing what changed.
    pub changes: Vec<String>,
}

impl Edit {
    /// Whether this edit would change anything at all.
    pub fn is_noop(&self) -> bool {
        self.changes.is_empty()
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
/// # Errors
///
/// Returns the parse error when the existing file is not valid TOML — we will
/// not append to a file we cannot read, because the user's own syntax error
/// would then look like ours.
pub fn connect(existing: &str, port: u16) -> Result<Edit, toml::de::Error> {
    existing.parse::<toml::Table>()?;

    let mut changes = Vec::new();
    let mut out = replace_our_block(existing, &block(port), &mut changes);

    match top_level_model_provider(&out) {
        Some((_, value)) if value == "ironwire" => {}
        Some((line_no, value)) => {
            let mut lines: Vec<String> = out.lines().map(str::to_string).collect();
            lines[line_no] = "model_provider = \"ironwire\"".to_string();
            lines.insert(line_no, format!("{PREVIOUS_MARKER} \"{value}\""));
            out = joined(&lines);
            changes.push(format!(
                "model_provider: \"{value}\" → \"ironwire\" (the old value is kept in a comment)"
            ));
        }
        None => {
            out = format!("model_provider = \"ironwire\"\n{out}");
            changes.push("model_provider = \"ironwire\" (added)".to_string());
        }
    }

    Ok(Edit {
        contents: out,
        changes,
    })
}

/// Compute the edit that undoes [`connect`].
///
/// # Errors
///
/// Returns the parse error when the existing file is not valid TOML.
pub fn disconnect(existing: &str) -> Result<Edit, toml::de::Error> {
    existing.parse::<toml::Table>()?;

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
        if let Some(previous) = trimmed.strip_prefix(PREVIOUS_MARKER) {
            restore = Some(previous.trim().trim_matches('"').to_string());
            continue;
        }
        lines.push(line.to_string());
    }

    // Put back whatever `model_provider` said before, or drop the line we added.
    let provider_line = lines
        .iter()
        .position(|l| is_top_level_assignment(l, "model_provider"));
    if let Some(index) = provider_line {
        match &restore {
            Some(previous) => {
                lines[index] = format!("model_provider = \"{previous}\"");
                changes.push(format!("model_provider: restored to \"{previous}\""));
            }
            None => {
                lines.remove(index);
                changes.push("model_provider: removed".to_string());
            }
        }
    }

    Ok(Edit {
        contents: joined(&lines),
        changes,
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
        if is_top_level_assignment(line, "model_provider") {
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
        .and_then(|table| Some(table.get("model_provider")?.as_str()? == "ironwire"))
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
    fn a_previous_provider_is_remembered_so_it_can_be_restored() {
        let edit = connect("model_provider = \"openai\"\n", 8463).expect("edits");
        assert!(edit.contents.contains(PREVIOUS_MARKER));
        assert_eq!(
            table(&edit.contents)["model_provider"].as_str(),
            Some("ironwire")
        );

        let undone = disconnect(&edit.contents).expect("edits");
        assert_eq!(
            table(&undone.contents)["model_provider"].as_str(),
            Some("openai"),
            "disconnect must put back what the user had"
        );
        assert!(!undone.contents.contains(BLOCK_HEADER));
        assert!(!undone.contents.contains(PREVIOUS_MARKER));
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
