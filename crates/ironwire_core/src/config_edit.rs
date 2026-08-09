//! Changing one setting in `config.toml` without destroying the rest of it.
//!
//! `config.toml` is a file the user owns. `ironwire init --write` generates it
//! with a comment above every setting precisely so the settings are
//! discoverable, and people edit it by hand afterwards. Round-tripping it
//! through a TOML serializer to change one value would silently delete those
//! comments, reorder the tables, and drop any key the struct does not model —
//! a rude trade for setting one line, and the reason `src/codex_config.rs`
//! already edits text rather than structures.
//!
//! So this edits *text*, and parses only to check that the result is still
//! valid and still says what was asked for. It lives in `core` because it is a
//! pure `&str -> String` transformation with no I/O: the caller reads and
//! writes the file.

use crate::config::{Config, PrivacyMode};

/// Why an edit could not be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// The file on disk is not valid TOML. Refused rather than repaired:
    /// appending to a file we cannot parse risks making it worse, and the user
    /// is better told which file to look at.
    Unparseable(String),
    /// The edit produced something that does not parse, or does not mean what
    /// was asked. A bug here, caught before it reaches the disk.
    WouldCorrupt(String),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unparseable(detail) => {
                write!(
                    f,
                    "config.toml is not valid TOML, so it was left alone: {detail}"
                )
            }
            Self::WouldCorrupt(detail) => {
                write!(
                    f,
                    "that change would have produced an unusable config.toml: {detail}"
                )
            }
        }
    }
}

impl std::error::Error for EditError {}

/// The value `mode` serialises as.
fn mode_name(mode: PrivacyMode) -> &'static str {
    match mode {
        PrivacyMode::Off => "off",
        PrivacyMode::Credentials => "credentials",
        PrivacyMode::Pii => "pii",
        PrivacyMode::Full => "full",
    }
}

/// Set `privacy.mode`, leaving every other byte of the file as it was.
///
/// The three shapes a real file comes in are all handled: a `[privacy]` table
/// with a `mode` line, one without, and no `[privacy]` table at all.
///
/// # Errors
///
/// [`EditError::Unparseable`] when the existing file is not valid TOML, and
/// [`EditError::WouldCorrupt`] when the result would not parse or would not
/// actually carry the requested mode.
pub fn set_privacy_mode(existing: &str, mode: PrivacyMode) -> Result<String, EditError> {
    // Refuse to touch a file we cannot read. Appending a table to something
    // already broken produces a second problem on top of the first.
    if !existing.trim().is_empty() {
        toml::from_str::<Config>(existing).map_err(|e| EditError::Unparseable(e.to_string()))?;
    }

    let line = format!("mode = \"{}\"", mode_name(mode));
    let edited = replace_or_insert(existing, &line);

    // Parse the result, and check it means what was asked. `PrivacyConfig::mode`
    // resolves the deprecated `enabled`/`secrets` pair as well as `mode`, so
    // this also catches a file where an old switch would have won.
    let parsed: Config =
        toml::from_str(&edited).map_err(|e| EditError::WouldCorrupt(e.to_string()))?;
    if parsed.privacy.mode() != mode {
        return Err(EditError::WouldCorrupt(format!(
            "the file would still resolve to `{}`",
            mode_name(parsed.privacy.mode())
        )));
    }
    Ok(edited)
}

/// Put `setting` in the `[privacy]` table, however that table currently exists.
fn replace_or_insert(existing: &str, setting: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_privacy = false;
    let mut replaced = false;
    let mut inserted = false;

    for raw in existing.lines() {
        let trimmed = raw.trim();

        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            // Leaving `[privacy]` without having seen a `mode` key: add one at
            // the end of the table rather than at the end of the file, so it
            // lands under the header it belongs to.
            if in_privacy && !replaced && !inserted {
                insert_before_trailing_blanks(&mut out, setting);
                inserted = true;
            }
            in_privacy = trimmed == "[privacy]";
            out.push(raw.to_string());
            continue;
        }

        // Only an active `mode` key counts. A commented-out one is left exactly
        // as it is — it is documentation, and the new line goes in beside it.
        if in_privacy && !replaced && is_key(trimmed, "mode") {
            let indent: String = raw.chars().take_while(|c| c.is_whitespace()).collect();
            out.push(format!("{indent}{setting}"));
            replaced = true;
            continue;
        }

        out.push(raw.to_string());
    }

    if in_privacy && !replaced && !inserted {
        insert_before_trailing_blanks(&mut out, setting);
        inserted = true;
    }

    if !replaced && !inserted {
        if !out.is_empty() && out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.push("[privacy]".to_string());
        out.push(setting.to_string());
    }

    let mut text = out.join("\n");
    // Files end with a newline. The original may or may not have; the result
    // always does, because every other writer in this codebase does.
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Append inside a table, above any blank lines that separate it from the next
/// one — so the setting stays visually inside the table it belongs to.
fn insert_before_trailing_blanks(out: &mut Vec<String>, setting: &str) {
    let mut at = out.len();
    while at > 0 && out[at - 1].trim().is_empty() {
        at -= 1;
    }
    out.insert(at, setting.to_string());
}

/// Whether a line assigns `key`, ignoring whitespace and comments.
fn is_key(trimmed: &str, key: &str) -> bool {
    if trimmed.starts_with('#') {
        return false;
    }
    trimmed
        .split_once('=')
        .is_some_and(|(name, _)| name.trim() == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode_of(text: &str) -> PrivacyMode {
        toml::from_str::<Config>(text)
            .expect("parses")
            .privacy
            .mode()
    }

    #[test]
    fn an_existing_mode_is_replaced_in_place() {
        let before = "[server]\nport = 8463\n\n[privacy]\nmode = \"off\"\n";
        let after = set_privacy_mode(before, PrivacyMode::Pii).expect("edits");
        assert_eq!(mode_of(&after), PrivacyMode::Pii);
        assert!(after.contains("port = 8463"));
        assert_eq!(
            after.matches("mode =").count(),
            1,
            "left a second mode line: {after}"
        );
    }

    /// The whole reason this edits text: a serializer round-trip would delete
    /// every one of these, and `ironwire init --write` exists to put them there.
    #[test]
    fn comments_and_unrelated_settings_survive() {
        let before = "\
# IronWire configuration
[server]
port = 8463          # the loopback port

[privacy]
# How much to substitute.
mode = \"off\"
named_values = [\"acme-corp\"]

[capture]
enabled = true
";
        let after = set_privacy_mode(before, PrivacyMode::Credentials).expect("edits");
        assert!(after.contains("# IronWire configuration"));
        assert!(after.contains("# How much to substitute."));
        assert!(after.contains("port = 8463          # the loopback port"));
        assert!(after.contains("named_values = [\"acme-corp\"]"));
        assert!(after.contains("[capture]"));
        assert_eq!(mode_of(&after), PrivacyMode::Credentials);
    }

    #[test]
    fn a_privacy_table_without_a_mode_gains_one_inside_itself() {
        let before = "[privacy]\nnamed_values = [\"x\"]\n\n[capture]\nenabled = true\n";
        let after = set_privacy_mode(before, PrivacyMode::Full).expect("edits");
        let privacy_at = after.find("[privacy]").expect("has the table");
        let capture_at = after.find("[capture]").expect("has the table");
        let mode_at = after.find("mode =").expect("has the key");
        assert!(
            privacy_at < mode_at && mode_at < capture_at,
            "the key landed outside its table: {after}"
        );
    }

    #[test]
    fn a_file_with_no_privacy_table_gains_one() {
        let before = "[server]\nport = 8463\n";
        let after = set_privacy_mode(before, PrivacyMode::Credentials).expect("edits");
        assert!(after.contains("[privacy]"));
        assert_eq!(mode_of(&after), PrivacyMode::Credentials);
        assert!(after.contains("port = 8463"));
    }

    #[test]
    fn an_empty_file_gains_the_whole_table() {
        let after = set_privacy_mode("", PrivacyMode::Pii).expect("edits");
        assert_eq!(mode_of(&after), PrivacyMode::Pii);
    }

    /// Only the `mode` inside `[privacy]` is the privacy one. The scan has to
    /// leave the rest of the file alone, including tables that come after it.
    #[test]
    fn only_the_key_inside_the_privacy_table_is_touched() {
        let before = "[privacy]\nmode = \"off\"\n\n[server]\nport = 8463\n";
        let after = set_privacy_mode(before, PrivacyMode::Full).expect("edits");
        assert_eq!(after.matches("mode =").count(), 1, "{after}");
        assert!(after.contains("port = 8463"));
        assert_eq!(mode_of(&after), PrivacyMode::Full);
    }

    /// A commented-out `mode` is documentation. Overwriting it would both lose
    /// the comment and leave the real setting unset.
    #[test]
    fn a_commented_out_mode_is_not_mistaken_for_the_setting() {
        let before = "[privacy]\n# mode = \"credentials\"\n";
        let after = set_privacy_mode(before, PrivacyMode::Pii).expect("edits");
        assert!(after.contains("# mode = \"credentials\""), "{after}");
        assert_eq!(mode_of(&after), PrivacyMode::Pii);
    }

    /// Refused rather than repaired: appending to a file we cannot parse turns
    /// one problem into two, and the user is better told which file to look at.
    #[test]
    fn a_config_we_cannot_parse_is_left_alone() {
        let outcome = set_privacy_mode("[server\nport = ", PrivacyMode::Pii);
        assert!(
            matches!(outcome, Err(EditError::Unparseable(_))),
            "{outcome:?}"
        );
    }

    /// The deprecated switches must not win over an explicit mode — a user who
    /// asked for `off` in a menu and got `credentials` because an old `enabled`
    /// line was still in the file would be right not to trust the menu again.
    #[test]
    fn the_deprecated_switches_do_not_override_the_mode_that_was_asked_for() {
        let before = "[privacy]\nenabled = true\nsecrets = true\n";
        let after = set_privacy_mode(before, PrivacyMode::Off).expect("edits");
        assert_eq!(mode_of(&after), PrivacyMode::Off, "{after}");
    }

    #[test]
    fn every_mode_round_trips_through_an_edit() {
        for mode in [
            PrivacyMode::Off,
            PrivacyMode::Credentials,
            PrivacyMode::Pii,
            PrivacyMode::Full,
        ] {
            let after = set_privacy_mode("[privacy]\nmode = \"off\"\n", mode).expect("edits");
            assert_eq!(mode_of(&after), mode);
        }
    }

    #[test]
    fn the_result_always_ends_with_a_newline() {
        for before in ["", "[server]\nport = 8463", "[privacy]\nmode = \"off\""] {
            let after = set_privacy_mode(before, PrivacyMode::Pii).expect("edits");
            assert!(after.ends_with('\n'), "{after:?}");
        }
    }

    /// Setting the mode twice is the same as setting it once — a menu that is
    /// clicked repeatedly must not accumulate lines.
    #[test]
    fn repeated_edits_do_not_accumulate_lines() {
        let mut text = "[server]\nport = 8463\n".to_string();
        for _ in 0..3 {
            text = set_privacy_mode(&text, PrivacyMode::Full).expect("edits");
        }
        assert_eq!(text.matches("[privacy]").count(), 1, "{text}");
        assert_eq!(text.matches("mode =").count(), 1, "{text}");
    }
}
