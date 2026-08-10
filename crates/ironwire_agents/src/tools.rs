//! Every tool IronWire knows about, and what is true of it right now.
//!
//! One list, from two sources that stay separate everywhere else: the agents
//! this binary ships knowing about, whose setup is more than one key and so
//! lives in code, and the ones a signed catalog introduced. A caller — the
//! control API, and through it the menu bar — should not have to know or care
//! which is which, only what is installed and what is pointed at us.
//!
//! Reading is all this does. Nothing here writes: `catalog::connect`,
//! `claude_settings::connect` and `codex_config::connect` are the only things
//! that touch a file, and they are called from the CLI where a change can be
//! printed before it happens.

use std::path::PathBuf;

use ironwire_catalog::schema::Catalog;

use crate::{catalog, claude_settings, codex_config};

/// A tool, as a screen needs to describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
    /// Stable id — what `ironwire disconnect <id>` accepts.
    pub id: String,
    /// What the user calls it.
    pub name: String,
    /// The file IronWire would edit. Shown because a tool nobody expected to be
    /// configured is a question about *which file*, every time.
    pub config_path: Option<PathBuf>,
    /// Whether the tool looks present on this machine.
    pub installed: bool,
    /// Whether its config currently routes through IronWire.
    pub wired: bool,
    /// What to run to point it here.
    pub connect_command: String,
}

/// Whether an executable of this name is reachable on `PATH`.
///
/// Lives here rather than in the CLI because detection is part of describing a
/// tool, and the control API needs the same answer the CLI gets.
#[must_use]
pub fn on_path(name: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file() || candidate.with_extension("exe").is_file()
    })
}

/// Every tool, built-in and catalog-described.
///
/// `installed` and `wired` are read from the filesystem, so this is a snapshot
/// rather than a subscription — which is right for a menu that opens, asks, and
/// closes again.
#[must_use]
pub fn all(catalog_document: &Catalog) -> Vec<Tool> {
    let mut tools = vec![built_in_claude(), built_in_codex()];

    for agent in catalog_document.agents() {
        let path = dirs::home_dir().map(|home| agent.config.resolve(&home));
        let contents = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_default();
        let installed =
            dirs::home_dir().is_some_and(|home| catalog::detected(agent, &home, &on_path));
        tools.push(Tool {
            id: agent.id.clone(),
            name: agent.name.clone(),
            config_path: path,
            installed,
            wired: catalog::is_wired(agent, &contents),
            connect_command: format!("ironwire connect {}", agent.id),
        });
    }
    tools
}

fn built_in_claude() -> Tool {
    let path = claude_settings::path();
    let contents = read(path.as_ref());
    Tool {
        id: "claude".to_string(),
        name: "Claude Code".to_string(),
        // A config directory it created, or its name on `PATH`. Both, because a
        // tool installed but never run has no directory yet, and one installed
        // as an app bundle leaves no name on `PATH`.
        installed: path
            .as_ref()
            .and_then(|path| path.parent().map(std::path::Path::exists))
            .unwrap_or(false)
            || on_path("claude"),
        wired: claude_settings::is_wired(&contents),
        config_path: path,
        connect_command: "ironwire connect claude".to_string(),
    }
}

fn built_in_codex() -> Tool {
    let path = codex_config::path();
    let contents = read(path.as_ref());
    Tool {
        id: "codex".to_string(),
        name: "Codex".to_string(),
        installed: path
            .as_ref()
            .and_then(|path| path.parent().map(std::path::Path::exists))
            .unwrap_or(false)
            || on_path("codex"),
        wired: codex_config::is_wired(&contents),
        config_path: path,
        connect_command: "ironwire connect codex".to_string(),
    }
}

/// A file that is not there reads as empty, which is the same answer as a file
/// with nothing of ours in it: not wired.
fn read(path: Option<&PathBuf>) -> String {
    path.and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_built_in_agents_are_always_listed() {
        // Listed whether or not they are installed: "Claude Code — not found"
        // is an answer, and omitting it looks like IronWire never heard of it.
        let tools = all(&Catalog::default());
        let ids: Vec<&str> = tools.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["claude", "codex"]);
    }

    #[test]
    fn every_tool_names_the_command_that_would_wire_it() {
        for tool in all(&Catalog::default()) {
            assert!(
                tool.connect_command.starts_with("ironwire connect "),
                "{tool:?}"
            );
        }
    }

    #[test]
    fn a_missing_file_is_not_wired_rather_than_an_error() {
        assert!(!claude_settings::is_wired(""));
        assert!(!codex_config::is_wired(""));
    }

    #[test]
    fn a_config_pointing_somewhere_else_is_not_wired() {
        assert!(!claude_settings::is_wired(
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://their-proxy.example"}}"#
        ));
        assert!(!codex_config::is_wired(
            "model_provider = \"someone-else\"\n"
        ));
    }

    #[test]
    fn a_config_pointing_at_us_is_wired() {
        assert!(claude_settings::is_wired(
            r#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:8463/anthropic"}}"#
        ));
        assert!(codex_config::is_wired("model_provider = \"ironwire\"\n"));
    }

    /// Codex's provider block can sit in the file unselected. What decides
    /// where traffic goes is `model_provider`, so that is what is reported.
    #[test]
    fn a_codex_provider_block_alone_is_not_wired() {
        let existing = "[model_providers.ironwire]\nbase_url = \"http://127.0.0.1:8463/openai\"\n";
        assert!(!codex_config::is_wired(existing));
    }
}
