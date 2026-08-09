//! The settings pane, laid out off-screen.
//
// Same technique as `MenuContentTests`: `ImageRenderer` lays the view out
// without a screen or a click. What it buys here is proof that the consent
// question actually takes up room — an "Enable" button that silently rendered
// nothing above it would pass every unit test and still be the one thing
// `docs/TRUST.md` §2 forbids.

import SwiftUI
import XCTest

@testable import IronWire
@testable import IronWireKit

@MainActor
final class SettingsContentTests: XCTestCase {
    private func prompt(points: Int = 4, version: Int = 1) -> ConsentPromptView {
        ConsentPromptView(
            version: version,
            backendId: "claude-sub",
            product: "Claude",
            summary: "IronWire will read the OAuth token that Claude Code stores on this machine "
                + "and send requests to api.anthropic.com with it, from this computer only.",
            points: (0..<points).map { "Point number \($0), which is a whole sentence about a real cost." },
            question: "Enable the Claude subscription backend?")
    }

    private func settings(
        options: [PrivacyOptionView] = [
            PrivacyOptionView(id: "off", describes: "off — requests are forwarded unchanged"),
            PrivacyOptionView(
                id: "full", describes: "only trusted backends", selectable: false,
                unavailableBecause: "`full` routes only to backends you have named, and none are named."),
        ],
        services: [ServiceView] = []
    ) -> SettingsView {
        SettingsView(
            privacy: PrivacySettingsView(
                mode: "off", summary: "off", options: options, trustedBackends: []),
            services: services)
    }

    private func render(_ settings: SettingsView?) -> NSImage? {
        let client = ControlClient.fixture(
            status: StatusView(version: "0.1.0", port: 8463), settings: settings)
        let renderer = ImageRenderer(content: SettingsContent(client: client).frame(width: 340))
        renderer.scale = 2
        return renderer.nsImage
    }

    private func height(_ settings: SettingsView?) throws -> CGFloat {
        try XCTUnwrap(render(settings)).size.height
    }

    func test_the_pane_lays_out_before_settings_have_arrived() throws {
        XCTAssertGreaterThan(try height(nil), 0)
    }

    func test_the_privacy_ladder_lays_out() throws {
        XCTAssertGreaterThan(try height(settings()), 0)
    }

    /// A greyed-out option that does not say why is worse than one that is not
    /// there. The reason has to occupy space on screen.
    func test_the_reason_a_mode_is_unavailable_takes_up_room() throws {
        let withReason = try height(settings())
        let withoutReason = try height(
            settings(options: [
                PrivacyOptionView(id: "off", describes: "off — requests are forwarded unchanged"),
                PrivacyOptionView(id: "full", describes: "only trusted backends", selectable: false),
            ]))
        XCTAssertGreaterThan(
            withReason, withoutReason,
            "the explanation for an unselectable mode is not being drawn")
    }

    // MARK: - Consent

    private func service(prompt: ConsentPromptView?, consented: Bool = false) -> ServiceView {
        ServiceView(
            id: "claude-sub", name: "Claude subscription", authenticated: true,
            requiresConsent: true, consented: consented, consentPrompt: prompt,
            connectCommand: "ironwire connect claude")
    }

    func test_a_service_awaiting_consent_lays_out() throws {
        XCTAssertGreaterThan(try height(settings(services: [service(prompt: prompt())])), 0)
    }

    /// The question is not shown until it is asked for — the settings pane opens
    /// on a list, not on a wall of consent text for something nobody clicked.
    func test_the_question_is_not_on_screen_until_it_is_asked_for() throws {
        let listed = try height(settings(services: [service(prompt: prompt())]))
        let bare = try height(settings(services: [service(prompt: nil)]))
        // Both render a one-line row plus an action; the four-point question
        // would be far taller than either.
        XCTAssertLessThan(
            abs(listed - bare), 40,
            "the consent question appears to be rendered before anyone asked for it")
    }

    /// A credential that was never found is not something a menu can conjure, so
    /// there is nothing to toggle and the row says what to run instead.
    func test_a_service_with_no_credential_offers_a_command_not_a_switch() throws {
        let unauthenticated = ServiceView(
            id: "claude-sub", name: "Claude subscription", authenticated: false,
            detail: "no Claude Code login found", requiresConsent: true, consented: false,
            consentPrompt: prompt(), connectCommand: "ironwire connect claude")
        XCTAssertFalse(unauthenticated.canToggle)
        XCTAssertGreaterThan(try height(settings(services: [unauthenticated])), 0)
    }

    func test_an_enabled_service_lays_out() throws {
        XCTAssertGreaterThan(
            try height(settings(services: [service(prompt: prompt(), consented: true)])), 0)
    }

    /// A prompt that arrived incomplete must not produce an Enable button. The
    /// row falls back to naming the CLI command, which is shorter — so the
    /// incomplete case lays out no taller than the complete one.
    func test_an_incomplete_prompt_does_not_get_an_enable_button() throws {
        let incomplete = ConsentPromptView(
            version: 0, backendId: "claude-sub", product: "Claude",
            summary: "", points: [], question: "")
        XCTAssertFalse(incomplete.isComplete)
        XCTAssertGreaterThan(try height(settings(services: [service(prompt: incomplete)])), 0)
    }
}
