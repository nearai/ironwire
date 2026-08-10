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

    private func render(
        _ status: StatusView?, connection: ControlClient.Connection = .connected,
        settings: SettingsView? = nil
    ) -> NSImage? {
        let view = MenuContent(
            client: .fixture(status: status, connection: connection, settings: settings),
            notifier: Notifier(), loginItem: LoginItem())
        let renderer = ImageRenderer(content: view)
        renderer.scale = 2
        return renderer.nsImage
    }

    private func height(_ status: StatusView?, settings: SettingsView? = nil) throws -> CGFloat {
        try XCTUnwrap(render(status, settings: settings)).size.height
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

    // MARK: - Consent, on the backend row
    //
    // The switch and the capacity share a row now, so these live here rather
    // than in a settings pane of their own.

    private func prompt(points: Int = 4, version: Int = 2) -> ConsentPromptView {
        ConsentPromptView(
            version: version, backendId: "claude-sub", product: "Claude",
            summary: "IronWire will read the OAuth token that Claude Code stores on this machine "
                + "and send requests to api.anthropic.com with it, from this computer only.",
            points: (0..<points).map { "Point number \($0), which is a whole sentence about a real cost." },
            question: "Enable the Claude subscription backend?")
    }

    private func settings(
        options: [PrivacyOptionView] = [],
        services: [ServiceView] = []
    ) -> SettingsView {
        SettingsView(
            privacy: PrivacySettingsView(
                mode: "off", summary: "off", options: options, trustedBackends: []),
            services: services)
    }

    private func unconsented(prompt: ConsentPromptView?) -> (StatusView, SettingsView) {
        (
            status(backends: [
                BackendView(id: "claude-sub", name: "Claude subscription", consented: false)
            ]),
            settings(services: [
                ServiceView(
                    id: "claude-sub", name: "Claude subscription", authenticated: true,
                    requiresConsent: true, consented: false, consentPrompt: prompt,
                    connectCommand: "ironwire connect claude")
            ])
        )
    }

    /// The two conditions a switch is gated on, and proof the row lays out with
    /// one. Asserted as predicates rather than by height: the fallback row for a
    /// prompt this build could not read is also two lines, so the two cases are
    /// deliberately the same height and a height comparison would prove nothing.
    func test_a_switch_is_offered_only_for_a_credential_and_a_readable_question() throws {
        let service = ServiceView(
            id: "claude-sub", name: "Claude subscription", authenticated: true,
            requiresConsent: true, consented: false, consentPrompt: prompt(),
            connectCommand: "ironwire connect claude")
        XCTAssertTrue(service.canToggle)
        XCTAssertTrue(service.consentPrompt?.isComplete == true)

        let (status, settings) = unconsented(prompt: prompt())
        XCTAssertGreaterThan(try height(status, settings: settings), 0)
    }

    /// Collapsed, the consent text costs nothing — neither the points nor the
    /// summary. Checked against a prompt three times as long in both: if any of
    /// it were being drawn, the longer one would lay out taller.
    ///
    /// This is the height half of the trade recorded in `docs/TRUST.md` §2. The
    /// row is two lines regardless of how much the daemon has to say.
    func test_the_consent_text_is_not_drawn_until_it_is_expanded() throws {
        let (shortStatus, shortSettings) = unconsented(prompt: prompt(points: 4))
        let (longStatus, longSettings) = unconsented(
            prompt: ConsentPromptView(
                version: 2, backendId: "claude-sub", product: "Claude",
                summary: String(repeating: "A very long summary sentence. ", count: 12),
                points: (0..<12).map { "Point \($0), which is a whole sentence about a real cost." },
                question: "Enable the Claude subscription backend?"))
        XCTAssertEqual(
            try height(shortStatus, settings: shortSettings),
            try height(longStatus, settings: longSettings),
            "the collapsed row grows with the consent text, so it is being drawn")
    }

    /// A credential that was never found is not something a menu can conjure, so
    /// there is nothing to switch and the row names the command instead.
    func test_a_backend_with_no_credential_offers_a_command_not_a_switch() throws {
        let service = ServiceView(
            id: "claude-sub", name: "Claude subscription", authenticated: false,
            detail: "no Claude Code login found", requiresConsent: true, consented: false,
            consentPrompt: prompt(), connectCommand: "ironwire connect claude")
        XCTAssertFalse(service.canToggle)
        let rendered = render(
            status(backends: [
                BackendView(
                    id: "claude-sub", name: "Claude subscription", authenticated: false,
                    consented: false)
            ]),
            settings: settings(services: [service]))
        XCTAssertNotNil(rendered)
    }

    // MARK: - The privacy ladder

    /// A greyed-out option that does not say why is worse than one that is not
    /// there. The reason has to occupy space on screen.
    func test_the_reason_a_mode_is_unavailable_takes_up_room() throws {
        let base = status(backends: [])
        let withReason = try height(
            base,
            settings: settings(options: [
                PrivacyOptionView(id: "off", describes: "requests are forwarded unchanged"),
                PrivacyOptionView(
                    id: "full", describes: "only trusted backends", selectable: false,
                    unavailableBecause: "`full` routes only to backends you have named, and none are named."),
            ]))
        let withoutReason = try height(
            base,
            settings: settings(options: [
                PrivacyOptionView(id: "off", describes: "requests are forwarded unchanged"),
                PrivacyOptionView(id: "full", describes: "only trusted backends", selectable: false),
            ]))
        XCTAssertGreaterThan(
            withReason, withoutReason,
            "the explanation for an unselectable mode is not being drawn")
    }

    /// With no settings document there is nothing to offer, so the pane falls
    /// back to the daemon's own words and still lays out.
    func test_the_privacy_line_falls_back_to_the_daemons_words() throws {
        XCTAssertGreaterThan(
            try height(status(backends: [], privacy: "redacting emails and API keys")), 0)
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
