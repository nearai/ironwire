//! Rendering the daemon's state for a terminal.
//!
//! One rule governs every number here: if the provider did not tell us, we say
//! `unknown` (`docs/CRITIQUE.md` §4). A plausible fabricated percentage costs
//! us belief in the numbers that *are* real, which is most of the value of this
//! screen.

use ironwire_proxy::control::{BackendView, HeadroomView, StatusView};

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
    out
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
        });
        assert!(rendered.contains("ironwire connect claude"));
    }
}
