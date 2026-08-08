//! Rendering the daemon's state for a terminal.
//!
//! One rule governs every number here: if the provider did not tell us, we say
//! `unknown` (`docs/CRITIQUE.md` §4). A plausible fabricated percentage costs
//! us belief in the numbers that *are* real, which is most of the value of this
//! screen.

use ironwire_ledger::{Exchange, Summary};
use ironwire_proxy::control::{BackendView, HeadroomView, LogView, StatusView};
use ironwire_update::UpdateStatus;

/// Render `ironwire log`.
#[must_use]
pub(crate) fn log(view: &LogView) -> String {
    let mut out = String::new();
    if !view.enabled {
        out.push_str("Local trace capture is off.\n\n");
        out.push_str("  Turn it on with `capture.enabled = true` in\n");
        out.push_str("  $IRONWIRE_HOME/config.toml, then restart the daemon.\n");
        return out;
    }
    if view.exchanges.is_empty() {
        out.push_str("No exchanges recorded yet.\n");
        return out;
    }

    out.push_str(&format!(
        "{:<21} {:<15} {:<18} {:>9} {:>8} {:>7}\n",
        "when", "backend", "model", "in/cached", "out", "took"
    ));
    for exchange in &view.exchanges {
        out.push_str(&exchange_row(exchange));
    }
    out.push('\n');
    out.push_str(&summary_block(&view.last_24h));
    out
}

fn exchange_row(exchange: &Exchange) -> String {
    let model = exchange
        .served_model
        .as_deref()
        .or(exchange.requested_model.as_deref())
        .unwrap_or("—");
    // An exchange whose usage the provider never reported shows a dash, not a
    // zero. A fabricated zero would understate what the user actually spent.
    let tokens = |value: Option<i64>| value.map_or_else(|| "—".to_string(), compact);
    let cached = exchange
        .cache_read_tokens
        .map_or_else(String::new, |v| format!("/{}", compact(v)));
    let took = exchange.total_ms.map_or_else(
        || "—".to_string(),
        |ms| format!("{:.1}s", ms as f64 / 1000.0),
    );
    let status = if exchange.status == 200 {
        String::new()
    } else {
        format!("  [{}]", exchange.status)
    };

    format!(
        "{:<21} {:<15} {:<18} {:>9} {:>8} {:>7}{status}\n",
        exchange.started_at.format("%Y-%m-%d %H:%M:%S"),
        truncate(&exchange.backend, 15),
        truncate(model, 18),
        format!("{}{}", tokens(exchange.input_tokens), cached),
        tokens(exchange.output_tokens),
        took,
    )
}

fn summary_block(summary: &Summary) -> String {
    let mut out = String::new();
    out.push_str(&format!("last 24h: {} exchanges", summary.exchanges));
    if summary.without_usage > 0 {
        out.push_str(&format!(
            " ({} with no usage reported)",
            summary.without_usage
        ));
    }
    out.push('\n');
    out.push_str(&format!(
        "  tokens: {} in · {} cached · {} out\n",
        compact(summary.input_tokens),
        compact(summary.cache_read_tokens),
        compact(summary.output_tokens),
    ));
    if !summary.by_backend.is_empty() {
        let split = summary
            .by_backend
            .iter()
            .map(|(backend, count)| format!("{backend} {count}"))
            .collect::<Vec<_>>()
            .join(" · ");
        out.push_str(&format!("  routed: {split}\n"));
    }
    out
}

fn compact(value: i64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    text.chars()
        .take(width.saturating_sub(1))
        .collect::<String>()
        + "…"
}

/// Render the full status screen.
#[must_use]
pub(crate) fn status(status: &StatusView) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "IronWire {} — http://127.0.0.1:{}\n\n",
        status.version, status.port
    ));

    if status.backends.is_empty() {
        out.push_str("No backends configured.\n\n");
        out.push_str("  Run `ironwire connect claude --subscription`, or set\n");
        out.push_str("  ANTHROPIC_API_KEY and restart the daemon.\n");
        return out;
    }

    for backend in &status.backends {
        out.push_str(&backend_block(backend));
        out.push('\n');
    }

    if let Some(pin) = &status.pin {
        out.push_str(&format!("Pinned to {pin} (clear with `ironwire pin`)\n"));
    }
    out.push_str(&format!(
        "{} conversation(s) with a sticky route\n",
        status.tracked_conversations
    ));
    if status.quirks_serial > 0 {
        out.push_str(&format!(
            "provider quirks: serial {}\n",
            status.quirks_serial
        ));
    }
    out.push_str(&update_line(&status.update));
    out
}

/// Notify-only: say a newer release exists and how to get it. Never act.
fn update_line(update: &UpdateStatus) -> String {
    match update {
        UpdateStatus::Available {
            latest,
            upgrade_command,
            ..
        } => match upgrade_command {
            Some(command) => format!("\nironwire {latest} is available — {command}\n"),
            None => format!("\nironwire {latest} is available\n"),
        },
        UpdateStatus::Unsupported {
            latest,
            minimum_supported,
            upgrade_command,
        } => {
            let how = upgrade_command
                .as_deref()
                .map_or_else(String::new, |c| format!(" — {c}"));
            format!(
                "\nThis build is below the supported floor ({minimum_supported}); providers may \n\
                 have changed in ways it does not handle. {latest} is available{how}\n"
            )
        }
        UpdateStatus::UpToDate | UpdateStatus::Unknown => String::new(),
    }
}

fn backend_block(backend: &BackendView) -> String {
    let mut out = String::new();
    out.push_str(&format!("{}\n", backend.name));

    if !backend.authenticated {
        let detail = backend.detail.as_deref().unwrap_or("not authenticated");
        out.push_str(&format!("  not connected — {detail}\n"));
        return out;
    }
    if !backend.consented {
        out.push_str("  awaiting consent — run `ironwire connect claude --subscription`\n");
        return out;
    }

    out.push_str(&format!(
        "  connected · {}\n",
        backend.kind.replace('_', " ")
    ));
    out.push_str(&format!("  capacity: {}\n", headroom(&backend.headroom)));
    if !backend.models.is_empty() {
        out.push_str(&format!("  models: {}\n", backend.models.join(", ")));
    }
    out
}

fn headroom(headroom: &HeadroomView) -> String {
    match headroom {
        HeadroomView::Observed {
            used_pct,
            observed_secs_ago,
            resets_in_secs,
        } => {
            let bar = meter(*used_pct);
            let age = duration(*observed_secs_ago);
            match resets_in_secs {
                Some(reset) if *reset > 0 => format!(
                    "{bar} {used_pct:.0}% used · resets in {} · observed {age} ago",
                    duration(*reset)
                ),
                _ => format!("{bar} {used_pct:.0}% used · observed {age} ago"),
            }
        }
        HeadroomView::Exhausted { retry_in_secs } => {
            format!("exhausted · retry in {}", duration(*retry_in_secs))
        }
        // Not "0%", not "healthy" — we genuinely do not know, and saying so is
        // what makes the other rows worth believing.
        HeadroomView::Unknown => "unknown (the provider has not reported yet)".to_string(),
    }
}

fn meter(used_pct: f32) -> String {
    const WIDTH: usize = 10;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "used_pct is clamped to 0..=100 at observation time"
    )]
    let filled = ((used_pct.clamp(0.0, 100.0) / 100.0) * WIDTH as f32).round() as usize;
    format!(
        "[{}{}]",
        "█".repeat(filled.min(WIDTH)),
        "░".repeat(WIDTH.saturating_sub(filled))
    )
}

fn duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3600 {
        return format!("{}m", secs / 60);
    }
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    if minutes == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h{minutes}m")
    }
}

#[cfg(test)]
mod log_tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn exchange() -> Exchange {
        Exchange {
            started_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).expect("timestamp"),
            ttfb_ms: Some(400),
            total_ms: Some(9_100),
            facade: "anthropic".into(),
            path: "/v1/messages".into(),
            conversation: "c".into(),
            backend: "claude-sub".into(),
            requested_model: Some("claude-opus-4-6".into()),
            served_model: Some("claude-opus-4-6".into()),
            rung: "preferred".into(),
            attempts: 1,
            input_tokens: Some(12),
            cache_read_tokens: Some(98_000),
            cache_write_tokens: Some(2_048),
            output_tokens: Some(137),
            status: 200,
            error: None,
        }
    }

    fn view(exchanges: Vec<Exchange>, last_24h: Summary) -> LogView {
        LogView {
            enabled: true,
            exchanges,
            last_24h,
        }
    }

    #[test]
    fn unreported_usage_renders_as_a_dash_not_a_zero() {
        // A fabricated zero understates what the user actually spent.
        let mut unknown = exchange();
        unknown.input_tokens = None;
        unknown.output_tokens = None;
        unknown.cache_read_tokens = None;
        let row = exchange_row(&unknown);
        assert!(row.contains('—'));
        assert!(
            !row.contains(" 0 "),
            "rendered a zero we never observed: {row}"
        );
    }

    #[test]
    fn a_row_shows_the_model_that_actually_served_it() {
        let mut substituted = exchange();
        substituted.requested_model = Some("claude-opus-4-6".into());
        substituted.served_model = Some("claude-sonnet-4-6".into());
        assert!(exchange_row(&substituted).contains("claude-sonnet-4-6"));
    }

    #[test]
    fn a_non_200_exchange_shows_its_status() {
        let mut failed = exchange();
        failed.status = 429;
        assert!(exchange_row(&failed).contains("[429]"));
        assert!(!exchange_row(&exchange()).contains('['));
    }

    #[test]
    fn the_summary_names_how_many_had_no_usage() {
        let rendered = summary_block(&Summary {
            exchanges: 10,
            without_usage: 3,
            input_tokens: 1_200,
            cache_read_tokens: 2_400_000,
            output_tokens: 900,
            by_backend: vec![("claude-sub".into(), 7), ("anthropic-key".into(), 3)],
        });
        assert!(rendered.contains("10 exchanges"));
        assert!(rendered.contains("3 with no usage reported"));
        assert!(rendered.contains("2.4M cached"));
        assert!(rendered.contains("claude-sub 7"));
    }

    #[test]
    fn capture_being_off_explains_how_to_turn_it_on() {
        let rendered = log(&LogView {
            enabled: false,
            exchanges: vec![],
            last_24h: Summary::default(),
        });
        assert!(rendered.contains("capture.enabled = true"));
    }

    #[test]
    fn an_empty_ledger_says_so_rather_than_printing_an_empty_table() {
        let rendered = log(&view(vec![], Summary::default()));
        assert!(rendered.contains("No exchanges recorded yet"));
    }

    #[test]
    fn compact_numbers_stay_readable() {
        assert_eq!(compact(42), "42");
        assert_eq!(compact(1_500), "1.5k");
        assert_eq!(compact(2_400_000), "2.4M");
    }

    #[test]
    fn long_names_are_truncated_without_panicking_on_multibyte_text() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a-very-long-backend-name", 10), "a-very-lo…");
        assert_eq!(truncate("日本語のモデル名です", 5), "日本語の…");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(headroom: HeadroomView) -> BackendView {
        BackendView {
            id: "claude-sub".into(),
            name: "Claude subscription".into(),
            kind: "subscription".into(),
            authenticated: true,
            consented: true,
            detail: None,
            headroom,
            models: vec!["claude-opus-4-6".into()],
        }
    }

    #[test]
    fn unknown_capacity_says_unknown_and_shows_no_number() {
        let rendered = backend_block(&view(HeadroomView::Unknown));
        assert!(rendered.contains("unknown"));
        assert!(!rendered.contains('%'), "we must not imply a measurement");
        assert!(!rendered.contains('█'));
    }

    #[test]
    fn an_observation_is_shown_with_its_age() {
        let rendered = backend_block(&view(HeadroomView::Observed {
            used_pct: 82.0,
            observed_secs_ago: 40,
            resets_in_secs: Some(8040),
        }));
        assert!(rendered.contains("82% used"));
        assert!(rendered.contains("observed 40s ago"));
        assert!(rendered.contains("resets in 2h14m"));
    }

    #[test]
    fn an_unauthenticated_backend_explains_itself_and_shows_no_capacity() {
        let mut backend = view(HeadroomView::Unknown);
        backend.authenticated = false;
        backend.detail = Some("Claude Code is not logged in on this machine".into());
        let rendered = backend_block(&backend);
        assert!(rendered.contains("not connected"));
        assert!(rendered.contains("not logged in"));
        assert!(!rendered.contains("capacity"));
    }

    #[test]
    fn an_unconsented_subscription_points_at_the_command_that_fixes_it() {
        let mut backend = view(HeadroomView::Unknown);
        backend.consented = false;
        assert!(backend_block(&backend).contains("ironwire connect claude --subscription"));
    }

    #[test]
    fn the_meter_tracks_the_percentage() {
        assert_eq!(meter(0.0), "[░░░░░░░░░░]");
        assert_eq!(meter(100.0), "[██████████]");
        assert_eq!(meter(50.0), "[█████░░░░░]");
        // Out-of-range input must not panic or overflow the bar.
        assert_eq!(meter(-5.0), "[░░░░░░░░░░]");
        assert_eq!(meter(150.0), "[██████████]");
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(45), "45s");
        assert_eq!(duration(600), "10m");
        assert_eq!(duration(7200), "2h");
        assert_eq!(duration(8040), "2h14m");
        assert_eq!(duration(-10), "0s");
    }

    #[test]
    fn an_empty_daemon_tells_you_what_to_run() {
        let rendered = status(&StatusView {
            version: "0.1.0".into(),
            port: 8463,
            tracked_conversations: 0,
            pin: None,
            backends: vec![],
            quirks_serial: 0,
            update: UpdateStatus::Unknown,
        });
        assert!(rendered.contains("ironwire connect claude"));
    }
}
