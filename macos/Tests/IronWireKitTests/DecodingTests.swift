//! What happens when the daemon and the app are not the same age.
//
// The daemon outlives the app that talks to it — it is a background service and
// this is a thing in a menu bar — so "the daemon is newer" is a state that will
// happen, not one to design against as an edge case. Every test here is about
// one field degrading instead of the whole menu going blank.

import XCTest

@testable import IronWireKit

final class DecodingTests: XCTestCase {
    private func decode<T: Decodable>(_ type: T.Type, _ json: String) throws -> T {
        try controlDecoder().decode(type, from: Data(json.utf8))
    }

    func test_a_full_status_document_decodes_every_field_the_menu_shows() throws {
        let status = try decode(StatusView.self, Fixtures.status)

        XCTAssertEqual(status.version, "0.1.0")
        XCTAssertEqual(status.port, 8463)
        XCTAssertEqual(status.backends.count, 2)
        XCTAssertEqual(status.backends[0].id, "claude-sub")
        XCTAssertEqual(status.backends[0].name, "Claude subscription")
        XCTAssertEqual(status.balance.available, 1)
        XCTAssertEqual(status.balance.spendTodayUsd, 0.16)
        XCTAssertEqual(status.privacy, "redacting emails and API keys")
        XCTAssertEqual(status.lastRoute?.backend, "nearai")
        XCTAssertEqual(status.lastRoute?.from, "claude-sub")
        XCTAssertEqual(status.lastRoute?.rung, .crossFamily)

        guard case .observed(let usedPct, let ago, let resets) = status.backends[0].headroom else {
            return XCTFail("expected an observation, got \(status.backends[0].headroom)")
        }
        XCTAssertEqual(usedPct, 82, accuracy: 0.001)
        XCTAssertEqual(ago, 40)
        XCTAssertEqual(resets, 1800)
    }

    func test_an_unobserved_backend_decodes_as_unknown_rather_than_as_a_number() throws {
        let backend = try decode(BackendView.self, #"{"id":"x","name":"X","headroom":{"state":"unknown"}}"#)
        XCTAssertEqual(backend.headroom, .unknown)
    }

    /// The property the whole `unrecognised` family exists for. A newer daemon
    /// adding a headroom state must cost one row's capacity line, not the menu.
    func test_a_headroom_state_from_a_newer_daemon_leaves_the_rest_of_the_backend_readable() throws {
        let backend = try decode(
            BackendView.self,
            #"""
            {"id":"x","name":"X","kind":"subscription","authenticated":true,"consented":true,
             "headroom":{"state":"throttled","until_secs":9},"health":{"circuit":"closed"},
             "models":["a","b"]}
            """#
        )
        XCTAssertEqual(backend.headroom, .unrecognised("throttled"))
        XCTAssertEqual(backend.name, "X")
        XCTAssertEqual(backend.models, ["a", "b"])
        XCTAssertNil(Format.capacityFraction(backend.headroom), "an unknown state must not draw a bar")
    }

    func test_an_update_state_from_a_newer_daemon_is_shown_as_nothing_rather_than_guessed_at() throws {
        let status = try decode(
            StatusView.self,
            #"{"version":"9.9.9","port":8463,"update":{"state":"rollback_required","latest":"9.9.8"}}"#
        )
        XCTAssertEqual(status.update, .unrecognised("rollback_required"))
        XCTAssertFalse(status.update.isActionable)
        XCTAssertEqual(status.version, "9.9.9")
    }

    func test_a_status_missing_everything_optional_still_decodes() throws {
        // An older daemon, or one that has not routed anything yet. The menu
        // has less to show; it still has a menu.
        let status = try decode(StatusView.self, #"{"version":"0.0.1","port":9000}"#)
        XCTAssertEqual(status.version, "0.0.1")
        XCTAssertEqual(status.port, 9000)
        XCTAssertTrue(status.backends.isEmpty)
        XCTAssertNil(status.lastRoute)
        XCTAssertNil(status.privacy)
        XCTAssertEqual(status.update, .unknown)
        XCTAssertEqual(status.balance.available, 0)
    }

    /// `rung` landed after `last_route` did, and `Rung::default()` is
    /// `Preferred`, so a daemon from before it must read as undegraded rather
    /// than as a fallback that never happened.
    func test_a_route_from_before_rung_existed_reads_as_the_undegraded_case() throws {
        let route = try decode(
            LastRouteView.self,
            #"{"backend":"claude-sub","model":null,"from":null,"at":"2026-08-08T12:00:00Z"}"#
        )
        XCTAssertEqual(route.rung, .preferred)
        XCTAssertFalse(route.rung.isDegraded)
    }

    func test_every_rung_the_daemon_can_serialise_is_understood() throws {
        let cases: [(String, Rung)] = [
            ("preferred", .preferred),
            ("smaller_model", .smallerModel),
            ("alternate_credential", .alternateCredential),
            ("cross_family", .crossFamily),
        ]
        for (raw, expected) in cases {
            let route = try decode(
                LastRouteView.self,
                #"{"backend":"b","rung":"\#(raw)","at":"2026-08-08T12:00:00Z"}"#
            )
            XCTAssertEqual(route.rung, expected, "for \(raw)")
        }
    }

    /// A rung this build has never heard of is a rung below `preferred`, because
    /// `preferred` is the only undegraded value and it is known. Reporting
    /// "fine" is the one answer that would certainly be wrong.
    func test_a_rung_from_a_newer_ladder_counts_as_degraded_but_not_as_an_alarm() throws {
        let route = try decode(
            LastRouteView.self,
            #"{"backend":"b","rung":"different_provider","at":"2026-08-08T12:00:00Z"}"#
        )
        XCTAssertEqual(route.rung, .unrecognised("different_provider"))
        XCTAssertTrue(route.rung.isDegraded)
        XCTAssertFalse(route.rung.isUserVisible, "only a cross-family descent interrupts anyone")
    }

    func test_an_event_type_from_a_newer_daemon_is_ignored_rather_than_fatal() throws {
        let event = try decode(Event.self, #"{"type":"quirks_updated","at":"2026-08-08T12:00:00Z","serial":7}"#)
        XCTAssertEqual(event, .unrecognised("quirks_updated"))
        XCTAssertNil(NotificationPolicy.notification(for: event))
    }

    func test_each_event_the_bus_publishes_decodes() throws {
        let routed = try decode(
            Event.self,
            #"""
            {"type":"routed","at":"2026-08-08T12:00:00Z","conversation":"abc123",
             "from":"claude-sub","to":"nearai","rung":"cross_family","translated":true,
             "reason":"no same-family capacity available"}
            """#
        )
        guard case .routed(_, let conversation, let from, let to, let rung, let translated, _) = routed else {
            return XCTFail("expected a route, got \(routed)")
        }
        XCTAssertEqual(conversation, "abc123")
        XCTAssertEqual(from, "claude-sub")
        XCTAssertEqual(to, "nearai")
        XCTAssertEqual(rung, .crossFamily)
        XCTAssertTrue(translated)

        let health = try decode(
            Event.self, #"{"type":"health","at":"2026-08-08T12:00:00Z","backend":"nearai","circuit":"open"}"#)
        guard case .health(_, let backend, let circuit) = health else {
            return XCTFail("expected health, got \(health)")
        }
        XCTAssertEqual(backend, "nearai")
        XCTAssertEqual(circuit, "open")

        let failed = try decode(
            Event.self,
            #"{"type":"failed","at":"2026-08-08T12:00:00Z","conversation":"abc","detail":"every backend is rate limited"}"#)
        guard case .failed(_, _, let detail) = failed else {
            return XCTFail("expected a failure, got \(failed)")
        }
        XCTAssertEqual(detail, "every backend is rate limited")

        let cap = try decode(
            Event.self,
            #"{"type":"cap_reached","at":"2026-08-08T12:00:00Z","backend":"openai","spent_usd":5.0,"cap_usd":5.0}"#)
        guard case .capReached(_, let capBackend, let spent, let capUsd) = cap else {
            return XCTFail("expected a cap, got \(cap)")
        }
        XCTAssertEqual(capBackend, "openai")
        XCTAssertEqual(spent, 5.0)
        XCTAssertEqual(capUsd, 5.0)
    }

    /// chrono emits however many fractional digits the value needs. Losing a
    /// timestamp costs a relative time; it must not cost the status around it.
    func test_a_timestamp_decodes_at_every_precision_chrono_emits() {
        let forms = [
            "2026-08-08T12:00:00Z",
            "2026-08-08T12:00:00.123Z",
            "2026-08-08T12:00:00.123456Z",
            "2026-08-08T12:00:00.123456789Z",
            "2026-08-08T14:00:00+02:00",
        ]
        for raw in forms {
            XCTAssertNotNil(Timestamp.parse(raw), "could not parse \(raw)")
        }
        XCTAssertNil(Timestamp.parse("not a time"))
    }

    func test_an_offset_timestamp_is_the_same_instant_as_its_utc_spelling() throws {
        let utc = try XCTUnwrap(Timestamp.parse("2026-08-08T12:00:00Z"))
        let offset = try XCTUnwrap(Timestamp.parse("2026-08-08T14:00:00+02:00"))
        XCTAssertEqual(utc, offset)
    }
}

enum Fixtures {
    /// Shaped exactly as `control.rs` serialises it, including the fields the
    /// app does not read.
    static let status = #"""
    {
      "version": "0.1.0",
      "port": 8463,
      "tracked_conversations": 3,
      "pin": null,
      "backends": [
        {
          "id": "claude-sub",
          "name": "Claude subscription",
          "kind": "subscription",
          "authenticated": true,
          "consented": true,
          "detail": null,
          "headroom": {"state": "observed", "used_pct": 82.0, "observed_secs_ago": 40, "resets_in_secs": 1800},
          "health": {"circuit": "closed", "consecutive_failures": 0, "retry_in_secs": null},
          "models": ["claude-opus-4", "claude-sonnet-4"]
        },
        {
          "id": "nearai",
          "name": "NEAR AI",
          "kind": "credits",
          "authenticated": true,
          "consented": true,
          "detail": null,
          "headroom": {"state": "unknown"},
          "health": {"circuit": "open", "consecutive_failures": 3, "retry_in_secs": 45},
          "models": ["qwen3-coder"]
        }
      ],
      "balance": {
        "available": 1,
        "free_available": 1,
        "unknown": 1,
        "unavailable": 0,
        "next_available_at": null,
        "spend_today_usd": 0.16,
        "cache_hit_rate": 0.72,
        "cache_exchanges": 41,
        "spend_cap": {"spent_usd": 0.16, "cap_usd": 5.0},
        "subscription_used": [{"name": "Claude subscription", "used_pct": 82.0, "exchanges": 41}]
      },
      "privacy": "redacting emails and API keys",
      "quirks_serial": 0,
      "update": {"state": "up_to_date"},
      "last_route": {
        "backend": "nearai",
        "model": "qwen3-coder",
        "from": "claude-sub",
        "rung": "cross_family",
        "at": "2026-08-08T12:00:00.123456789Z"
      },
      "usage": {"sessions": []}
    }
    """#
}
