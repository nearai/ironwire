//! `ironwire statusline` — one line of IronWire state, inside the agent's own UI.
//!
//! IronWire deliberately does not write into a response stream: the only text
//! channel there is the model's own, and putting words in it would be putting
//! words in the model's mouth (`docs/DESIGN.md` §7). A status line is the
//! channel that does not have that problem — it is the harness's own furniture,
//! rendered outside the transcript, and the harness invites us into it.
//!
//! Claude Code runs the command in `statusLine` on every render and hands it a
//! JSON document on stdin (session, model, cost, rate limits). This command
//! reads that when it is there, asks the daemon what it is actually doing, and
//! prints a single line.
//!
//! Three properties matter more than what it says:
//!
//! 1. **It never blocks the UI.** This runs on every render of somebody's
//!    editor. The control call has a short timeout and every failure path
//!    prints nothing rather than an error — a status line that stalls or shouts
//!    is worse than no status line.
//! 2. **It never invents.** Same rule as `ironwire status`: a number we were
//!    not told is not printed (`docs/CRITIQUE.md` §4).
//! 3. **It is quiet when things are normal**, so that the moments when it is
//!    not are worth reading.

use anyhow::Result;
use ironwire_proxy::control::{HeadroomView, StatusView};
use ironwire_update::UpdateStatus;

use super::control_client::ControlClient;

/// How long the client's JSON, and the daemon, get before we give up and print
/// what we have. Chosen against the render loop this sits in, not against the
/// network.
const BUDGET: std::time::Duration = std::time::Duration::from_millis(400);

/// What the harness told us about itself, of the parts we use.
#[derive(Debug, Default, serde::Deserialize)]
struct ClientContext {
    #[serde(default)]
    model: Option<ClientModel>,
}

#[derive(Debug, serde::Deserialize)]
struct ClientModel {
    #[serde(default)]
    id: Option<String>,
}

/// Render the line.
pub(crate) async fn run(port: Option<u16>) -> Result<()> {
    // Whatever we print, we exit 0: a status line that reports its own failure
    // into someone's editor has made their problem worse, not better.
    if let Some(line) = line(port).await {
        println!("{line}");
    }
    Ok(())
}

async fn line(port: Option<u16>) -> Option<String> {
    let client_context = read_client_context();
    let status = tokio::time::timeout(BUDGET, async {
        ControlClient::new(port).ok()?.status().await.ok()
    })
    .await
    .ok()
    .flatten()?;

    let mut parts = vec![routing(&status, client_context.as_ref())?];
    if let Some(switch) = switched(&status) {
        parts.push(switch);
    }
    parts.extend(capacity(&status));
    if let Some(update) = update(&status.update) {
        parts.push(update);
    }
    Some(parts.join(" · "))
}

/// How long a route change stays worth mentioning.
///
/// A fallback matters most in the minutes after it happens, when the user can
/// still connect it to what they were doing. Left up permanently it becomes
/// furniture, and stops being read at exactly the moment it matters again.
const SWITCH_VISIBLE_FOR: chrono::Duration = chrono::Duration::minutes(10);

/// The line's reason for existing: say when traffic moved, and from where.
fn switched(status: &StatusView) -> Option<String> {
    let route = status.last_route.as_ref()?;
    let from = route.from.as_deref()?;
    (chrono::Utc::now() - route.at < SWITCH_VISIBLE_FOR).then(|| format!("switched from {from}"))
}

/// Where traffic is going, and whether that is where the client thinks.
fn routing(status: &StatusView, client: Option<&ClientContext>) -> Option<String> {
    let serving = status
        .backends
        .iter()
        .find(|b| b.authenticated && b.consented)?;

    // The client names the model it asked for; we know what actually served it.
    // Printing the second only when it differs keeps the normal case short and
    // makes a substitution impossible to miss — which is the case the user
    // would otherwise discover from a bill or a worse answer.
    let asked = client
        .and_then(|c| c.model.as_ref())
        .and_then(|m| m.id.as_deref());
    let served = status.last_route.as_ref();
    match (asked, served) {
        (Some(asked), Some(route)) if route.model.as_deref().is_some_and(|m| m != asked) => {
            Some(format!(
                "ironwire → {} ({} for {asked})",
                route.backend,
                route.model.as_deref().unwrap_or("?")
            ))
        }
        (_, Some(route)) => Some(format!("ironwire → {}", route.backend)),
        (_, None) => Some(format!("ironwire → {}", serving.id)),
    }
}

/// Capacity, but only for the pools that have actually reported one, and only
/// when there is something to say about them.
fn capacity(status: &StatusView) -> Vec<String> {
    status
        .backends
        .iter()
        .filter(|b| b.authenticated && b.consented)
        .filter_map(|backend| match backend.headroom {
            // Below a third used is not news. A status line that always shows a
            // number teaches people to stop seeing it.
            HeadroomView::Observed { used_pct, .. } if used_pct >= 33.0 => {
                Some(format!("{} {used_pct:.0}%", short_name(&backend.name)))
            }
            HeadroomView::Exhausted { .. } => {
                Some(format!("{} exhausted", short_name(&backend.name)))
            }
            _ => None,
        })
        .collect()
}

/// Notify-only, exactly as everywhere else: say a release exists, never act
/// on it (`docs/UPDATES.md`).
fn update(status: &UpdateStatus) -> Option<String> {
    match status {
        UpdateStatus::Available { latest, .. } => Some(format!("ironwire {latest} available")),
        UpdateStatus::Unsupported { latest, .. } => Some(format!(
            "ironwire {latest} available (this build is unsupported)"
        )),
        UpdateStatus::UpToDate | UpdateStatus::Unknown => None,
    }
}

/// "Claude subscription" is too wide for a line shared with someone's cwd.
fn short_name(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or(name)
}

/// Read the harness's JSON from stdin, if it sent any.
///
/// Not every harness does, and a status line invoked by hand has a terminal on
/// stdin rather than a document — so this reads only when stdin is not a TTY,
/// and treats anything unparseable as "no context" rather than an error.
fn read_client_context() -> Option<ClientContext> {
    use std::io::{IsTerminal, Read};
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let mut buffer = String::new();
    stdin.read_to_string(&mut buffer).ok()?;
    serde_json::from_str(&buffer).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_document_we_do_not_fully_model_still_yields_its_model() {
        // The harness will add fields; parsing must not become all-or-nothing
        // over ones we never asked about.
        let parsed: ClientContext = serde_json::from_str(
            r#"{"session_id":"x","model":{"id":"claude-opus-5","display_name":"Opus 5"},
                "cost":{"total_cost_usd":0},"something_new":{"nested":true}}"#,
        )
        .expect("parses");
        assert_eq!(
            parsed.model.and_then(|m| m.id).as_deref(),
            Some("claude-opus-5")
        );
    }

    #[test]
    fn a_document_without_a_model_is_not_an_error() {
        let parsed: ClientContext = serde_json::from_str(r#"{"session_id":"x"}"#).expect("parses");
        assert!(parsed.model.is_none());
    }

    #[test]
    fn an_up_to_date_build_says_nothing() {
        assert!(update(&UpdateStatus::UpToDate).is_none());
        assert!(update(&UpdateStatus::Unknown).is_none());
    }

    #[test]
    fn a_subscription_name_is_shortened_to_its_first_word() {
        assert_eq!(short_name("Claude subscription"), "Claude");
        assert_eq!(short_name("ChatGPT subscription"), "ChatGPT");
    }

    fn status_with(last_route: Option<ironwire_proxy::control::LastRouteView>) -> StatusView {
        StatusView {
            version: "0.1.0".into(),
            port: 8463,
            tracked_conversations: 1,
            pin: None,
            backends: vec![],
            balance: ironwire_proxy::control::BalanceView::default(),
            privacy: None,
            quirks_serial: 0,
            update: UpdateStatus::UpToDate,
            last_route,
            usage: ironwire_usage::UsageReport::default(),
        }
    }

    fn route(from: Option<&str>, ago: chrono::Duration) -> ironwire_proxy::control::LastRouteView {
        ironwire_proxy::control::LastRouteView {
            backend: "nearai".into(),
            model: None,
            from: from.map(ToString::to_string),
            rung: ironwire_core::policy::Rung::CrossFamily,
            at: chrono::Utc::now() - ago,
        }
    }

    #[test]
    fn a_route_that_did_not_change_is_not_announced() {
        // The ordinary case is a conversation staying put, and saying so every
        // render is how a status line stops being read.
        assert!(switched(&status_with(Some(route(None, chrono::Duration::zero())))).is_none());
        assert!(switched(&status_with(None)).is_none());
    }

    #[test]
    fn a_recent_switch_names_where_it_came_from() {
        let line = switched(&status_with(Some(route(
            Some("claude-sub"),
            chrono::Duration::minutes(1),
        ))))
        .expect("announced");
        assert!(line.contains("claude-sub"), "got: {line}");
    }

    /// A fallback from an hour ago is history, not status. Left up it becomes
    /// furniture and stops being read at the moment it matters again.
    #[test]
    fn an_old_switch_stops_being_news() {
        assert!(
            switched(&status_with(Some(route(
                Some("claude-sub"),
                chrono::Duration::hours(1),
            ))))
            .is_none()
        );
    }
}
