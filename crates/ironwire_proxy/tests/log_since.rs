//! `GET /_ironwire/log?since=` returns a window, oldest first.
//!
//! The default view is the newest 20, which is right for a person reading
//! `ironwire log`. It is wrong for anything polling the ledger: newest-first
//! plus a limit drops the *oldest* rows in a window, and a reader that has
//! already advanced past them cannot tell that it missed any. So a windowed
//! read pages forward instead -- oldest first, truncated at the far end, where
//! the next request picks up what was cut.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use ironwire_core::config::Config;
use ironwire_creds::ConsentLedger;
use ironwire_ledger::{Exchange, Ledger};
use ironwire_proxy::control::LogView;
use ironwire_proxy::server::app;
use ironwire_proxy::state::{AppState, BackendRegistry};
use tower::ServiceExt;

fn at(offset: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000 + offset, 0).expect("valid timestamp")
}

fn exchange(started_at: DateTime<Utc>) -> Exchange {
    Exchange {
        started_at,
        ttfb_ms: None,
        total_ms: Some(10),
        facade: "anthropic".into(),
        path: "/v1/messages".into(),
        conversation: "c-1".into(),
        client_session_id: Some(format!("s-{}", started_at.timestamp())),
        backend: "claude-sub".into(),
        requested_model: None,
        served_model: None,
        rung: "same_model".into(),
        attempts: 1,
        input_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        output_tokens: None,
        cost_usd: None,
        substitutions: None,
        status: 200,
        error: None,
    }
}

async fn fetch(ledger: Ledger, query: &str) -> LogView {
    let state = AppState::new(
        BackendRegistry::new(),
        Config::default(),
        ConsentLedger::default(),
        "test-token".to_string(),
    )
    .with_ledger(Some(ledger));
    let response = app(state)
        .oneshot(
            Request::builder()
                .uri(format!("/_ironwire/log{query}"))
                .header("authorization", "Bearer test-token")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("served");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("a LogView")
}

fn seeded() -> Ledger {
    let ledger = Ledger::in_memory().expect("ledger opens");
    for offset in 0..5 {
        ledger.record(&exchange(at(offset))).expect("record");
    }
    ledger
}

#[tokio::test]
async fn a_window_starts_at_the_instant_it_names_and_runs_forward() {
    let view = fetch(
        seeded(),
        &format!(
            "?since={}",
            at(2).to_rfc3339_opts(SecondsFormat::Secs, true)
        ),
    )
    .await;
    let seen: Vec<_> = view
        .exchanges
        .iter()
        .map(|e| e.started_at.timestamp())
        .collect();
    assert_eq!(
        seen,
        vec![at(2).timestamp(), at(3).timestamp(), at(4).timestamp()],
        "the window excludes what came before it, oldest first"
    );
}

#[tokio::test]
async fn a_truncated_window_cuts_the_end_the_caller_has_not_reached() {
    // The rows dropped must be the newest, so the next request returns them.
    // Dropping the oldest would be a gap no poller can detect.
    let view = fetch(
        seeded(),
        &format!(
            "?since={}&limit=2",
            at(0).to_rfc3339_opts(SecondsFormat::Secs, true)
        ),
    )
    .await;
    let seen: Vec<_> = view
        .exchanges
        .iter()
        .map(|e| e.started_at.timestamp())
        .collect();
    assert_eq!(seen, vec![at(0).timestamp(), at(1).timestamp()]);
}

#[tokio::test]
async fn without_a_window_the_log_is_still_newest_first() {
    // The default view is what a person reads, and it must not change.
    let view = fetch(seeded(), "?limit=2").await;
    let seen: Vec<_> = view
        .exchanges
        .iter()
        .map(|e| e.started_at.timestamp())
        .collect();
    assert_eq!(seen, vec![at(4).timestamp(), at(3).timestamp()]);
}

#[tokio::test]
async fn the_windowed_log_still_needs_the_control_token() {
    let state = AppState::new(
        BackendRegistry::new(),
        Config::default(),
        ConsentLedger::default(),
        "test-token".to_string(),
    )
    .with_ledger(Some(seeded()));
    let response = app(state)
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/_ironwire/log?since={}",
                    at(0).to_rfc3339_opts(SecondsFormat::Secs, true)
                ))
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("served");
    assert_ne!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_summary_still_covers_the_last_day_regardless_of_the_window() {
    // `last_24h` answers "what has this machine been doing", which is not the
    // same question as the window, and must not follow it.
    let ledger = Ledger::in_memory().expect("ledger opens");
    ledger
        .record(&exchange(Utc::now() - Duration::hours(1)))
        .expect("record");
    let view = fetch(
        ledger,
        &format!(
            "?since={}",
            (Utc::now() + Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true)
        ),
    )
    .await;
    assert!(view.exchanges.is_empty(), "the window is in the future");
    assert_eq!(view.last_24h.exchanges, 1, "the summary is unaffected");
}
