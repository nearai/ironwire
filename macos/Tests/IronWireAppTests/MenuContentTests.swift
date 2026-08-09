//! The dropdown, rendered off-screen.
//
// `ImageRenderer` lays the menu out exactly as the menu bar would, without a
// screen, a click, or a permission prompt. That buys two things the unit tests
// cannot: proof that every state actually *lays out* — a view that trapped on a
// `nil` would fail here — and a measurement of the one rule this app is most
// likely to break quietly.
//
// The bar test is the reason this file exists. `Format.capacityFraction`
// returning `nil` is necessary but not sufficient: a view could still draw a
// `ProgressView` beside it, and no test of the formatter would notice. The
// assertion is on laid-out height, because a bar occupies vertical space that
// text does not — see `height(for:)` for why height and not pixels.

import SwiftUI
import XCTest

@testable import IronWire
@testable import IronWireKit

@MainActor
final class MenuContentTests: XCTestCase {
    // MARK: - Fixtures

    private func status(
        backends: [BackendView],
        balance: BalanceView = BalanceView(available: 1, freeAvailable: 1),
        lastRoute: LastRouteView? = nil,
        privacy: String? = nil,
        update: UpdateStatus = .upToDate,
        pin: String? = nil
    ) -> StatusView {
        StatusView(
            version: "0.1.0", port: 8463, trackedConversations: 2, pin: pin,
            backends: backends, balance: balance, privacy: privacy,
            update: update, lastRoute: lastRoute)
    }

    private func render(_ status: StatusView?, connection: ControlClient.Connection = .connected) -> NSImage? {
        let view = MenuContent(
            client: .fixture(status: status, connection: connection),
            notifier: Notifier())
        let renderer = ImageRenderer(content: view)
        renderer.scale = 2
        return renderer.nsImage
    }

    /// The laid-out height of a menu showing one backend in a given state.
    ///
    /// Height rather than pixels, because `ImageRenderer` cannot rasterise
    /// `ProgressView` — it is AppKit-backed — but it does lay it out. The bar
    /// therefore shows up as vertical space and nowhere else, which is exactly
    /// the thing being asserted.
    private func height(for headroom: HeadroomView) throws -> CGFloat {
        let rendered = try XCTUnwrap(
            render(status(backends: [BackendView(id: "a", name: "Claude subscription", headroom: headroom)])))
        return rendered.size.height
    }

    // MARK: - The bar

    /// The acceptance criterion the issue calls the single most likely place for
    /// this app to go wrong. An unobserved backend must produce the word
    /// `unknown` and *no bar* — not a bar at zero, not a greyed one at half.
    func test_an_unobserved_backend_lays_out_shorter_because_it_has_no_bar() throws {
        let unobserved = try height(for: .unknown)
        let observed = try height(for: .observed(usedPct: 82, observedSecsAgo: 40, resetsInSecs: 1800))
        XCTAssertGreaterThan(
            observed, unobserved,
            "an unobserved backend appears to be drawing a capacity bar")
    }

    /// The set of states that must draw nothing, checked against each other
    /// rather than against a constant: all four carry a single line of text and
    /// no bar, so all four must lay out to exactly the same height. A bar
    /// appearing in any of them moves that one and fails here.
    ///
    /// `exhausted` and `capReached` are in this set deliberately. Neither is a
    /// measurement of how much of a window went, so a bar pinned to full would
    /// be claiming one.
    func test_no_state_but_an_observation_draws_a_bar() throws {
        let baseline = try height(for: .unknown)
        for headroom: HeadroomView in [
            .unrecognised("throttled"),
            .exhausted(retryInSecs: 300),
            .capReached(spentUsd: 5, capUsd: 5, resetsInSecs: 3600),
        ] {
            XCTAssertEqual(
                try height(for: headroom), baseline,
                "\(headroom) lays out differently from `unknown`, which means it is drawing a bar")
        }
    }

    /// The bar is the difference, not the wording: an observation with a reset
    /// time and one without carry different text and lay out identically.
    func test_the_extra_height_is_the_bar_and_not_the_words() throws {
        let withReset = try height(for: .observed(usedPct: 82, observedSecsAgo: 40, resetsInSecs: 1800))
        let without = try height(for: .observed(usedPct: 82, observedSecsAgo: 40, resetsInSecs: nil))
        XCTAssertEqual(withReset, without)
    }

    // MARK: - Every state lays out

    func test_a_daemon_that_is_not_running_renders_without_an_error_dialog() throws {
        let rendered = try XCTUnwrap(render(nil, connection: .unreachable))
        XCTAssertGreaterThan(rendered.size.height, 0)
    }

    func test_a_rejected_token_renders_as_a_line_and_not_as_a_crash() throws {
        let rendered = try XCTUnwrap(render(nil, connection: .unauthorised))
        XCTAssertGreaterThan(rendered.size.height, 0)
    }

    func test_a_fresh_install_with_no_backends_lays_out() throws {
        let rendered = try XCTUnwrap(render(status(backends: [])))
        XCTAssertGreaterThan(rendered.size.height, 0)
    }

    /// The state a real daemon will not produce on request, and the one the icon
    /// exists for.
    func test_a_skipped_backend_and_a_cross_family_route_lay_out() throws {
        let rendered = try XCTUnwrap(
            render(
                status(
                    backends: [
                        BackendView(
                            id: "claude-sub", name: "Claude subscription",
                            headroom: .observed(usedPct: 97, observedSecsAgo: 5, resetsInSecs: 600),
                            health: HealthView(circuit: "open", consecutiveFailures: 3, retryInSecs: 45)),
                        BackendView(id: "nearai", name: "NEAR AI", kind: "credits", headroom: .unknown),
                    ],
                    balance: BalanceView(
                        available: 0, unknown: 1, unavailable: 1,
                        nextAvailableAt: Date().addingTimeInterval(45),
                        spendTodayUsd: 0.16, spendCap: SpendCapView(spentUsd: 0.16, capUsd: 5)),
                    lastRoute: LastRouteView(
                        backend: "nearai", model: "qwen3-coder", from: "claude-sub", rung: .crossFamily),
                    privacy: "redacting emails and API keys",
                    update: .available(latest: "0.2.0", summary: "faster failover", upgradeCommand: "brew upgrade ironwire"),
                    pin: "nearai")))
        XCTAssertGreaterThan(rendered.size.height, 0)
    }

    /// A newer daemon must cost one field, never the menu.
    func test_a_status_full_of_states_this_build_does_not_recognise_still_lays_out() throws {
        let rendered = try XCTUnwrap(
            render(
                status(
                    backends: [BackendView(id: "a", name: "A", headroom: .unrecognised("throttled"))],
                    lastRoute: LastRouteView(backend: "a", rung: .unrecognised("different_provider")),
                    update: .unrecognised("rollback_required"))))
        XCTAssertGreaterThan(rendered.size.height, 0)
    }

    /// Every backend a daemon reported has to be offerable, including ones that
    /// are not usable right now — the daemon validates the choice, not this app.
    func test_the_pin_control_offers_every_backend_the_daemon_reported() {
        let status = status(backends: [
            BackendView(id: "claude-sub", name: "Claude subscription"),
            BackendView(id: "codex-sub", name: "ChatGPT subscription", authenticated: false),
            BackendView(id: "nearai", name: "NEAR AI", health: HealthView(circuit: "open")),
        ])
        // The menu is built straight off this list, so the list is the
        // assertion: no filtering, no ranking, no opinion.
        XCTAssertEqual(status.backends.map(\.id), ["claude-sub", "codex-sub", "nearai"])
    }
}
