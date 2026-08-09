//! The settings pane.
//
// Three things can be changed from here: the privacy mode, whether a
// subscription backend is enabled, and where traffic is pinned. All three are
// daemon decisions — this offers what the daemon said is offerable and posts
// back what the user picked.
//
// The consent flow is the part worth reading carefully. `docs/TRUST.md` §2 says
// a subscription stays off until the user answers a specific question in plain
// language, and that the answer is recorded against the version of the question
// they were asked. So enabling one here shows the daemon's own wording in full —
// every point, in its order — and only then sends the answer, with the version
// that was on screen. There is no one-click enable, and there is deliberately no
// summarised version of the prompt: an abridged consent question is a different
// question.

import AppKit
import IronWireKit
import SwiftUI

struct SettingsContent: View {
    @ObservedObject var client: ControlClient

    /// The service whose consent question is currently open, if any.
    @State private var asking: ServiceView?
    @State private var error: String?
    @State private var warning: String?
    @State private var busy = false

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            if let settings = client.settings {
                privacySection(settings.privacy)
                servicesSection(settings.services)
            } else {
                Text("Loading settings…")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            if let warning {
                Text(warning)
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
            if let error {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.red)
            }
        }
        .padding(12)
        .task { await client.refreshSettings() }
    }

    // MARK: - Privacy

    @ViewBuilder
    private func privacySection(_ privacy: PrivacySettingsView) -> some View {
        section("Privacy filter") {
            // Verbatim, always. The daemon says what the filter is *doing*; a
            // word added here would be a claim about what the user is safe
            // from (`docs/TRUST.md` I7).
            Text(privacy.summary)
                .font(.caption)
                .foregroundStyle(.secondary)
                .textSelection(.enabled)

            ForEach(privacy.options) { option in
                privacyOption(option, current: privacy.mode)
            }
        }
    }

    private func privacyOption(_ option: PrivacyOptionView, current: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Button {
                apply(mode: option.id)
            } label: {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Image(systemName: option.id == current ? "largecircle.fill.circle" : "circle")
                        .foregroundStyle(option.id == current ? Color.accentColor : .secondary)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(option.id).font(.caption.weight(.medium))
                        Text(option.describes)
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    Spacer()
                }
            }
            .buttonStyle(.plain)
            // The daemon decides. `full` with nothing trusted would take every
            // backend out of service, and that rule lives in `Config::validate`.
            .disabled(!option.selectable || busy || option.id == current)
            .opacity(option.selectable ? 1 : 0.5)

            // A greyed-out option that does not say why is worse than one that
            // is not there at all.
            if let because = option.unavailableBecause {
                Text(because)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.leading, 20)
            }
        }
    }

    private func apply(mode: String) {
        busy = true
        Task {
            error = nil
            warning = nil
            switch await client.setPrivacyMode(mode) {
            case .success(let outcome):
                // Applied, but not saved. Said plainly rather than swallowed:
                // the user would otherwise find the old mode back after a
                // restart with no idea why.
                warning = outcome.warning
            case .failure(let failure):
                error = failure.message
            }
            busy = false
        }
    }

    // MARK: - Services

    @ViewBuilder
    private func servicesSection(_ services: [ServiceView]) -> some View {
        if !services.isEmpty {
            section("Services") {
                ForEach(services) { service in
                    serviceRow(service)
                }
            }
        }
    }

    private func serviceRow(_ service: ServiceView) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                Text(service.name).font(.caption.weight(.medium))
                Spacer()
                Text(Format.kind(service.kind))
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }

            Text(state(of: service))
                .font(.caption2)
                .foregroundStyle(colour(for: service))

            if asking?.id == service.id, let prompt = service.consentPrompt {
                consentQuestion(prompt, for: service)
            } else {
                serviceActions(service)
            }
        }
        .padding(.bottom, 2)
    }

    private func state(of service: ServiceView) -> String {
        if !service.authenticated {
            // A credential this app cannot conjure. It comes from the user
            // logging into Claude Code or Codex, or exporting an API key.
            return service.detail ?? "no credential found on this machine"
        }
        if service.requiresConsent && !service.consented {
            return "credential found, not enabled"
        }
        return "enabled"
    }

    private func colour(for service: ServiceView) -> Color {
        if !service.authenticated { return .secondary }
        if service.requiresConsent && !service.consented { return .orange }
        return .green
    }

    @ViewBuilder
    private func serviceActions(_ service: ServiceView) -> some View {
        HStack(spacing: 10) {
            if service.canToggle {
                if service.consented {
                    Button("Turn off") { setConsent(service, granted: false, version: 0) }
                        .disabled(busy)
                } else if service.consentPrompt?.isComplete == true {
                    Button("Enable…") { asking = service }
                        .disabled(busy)
                } else {
                    // A prompt we could not read in full is not one to collect
                    // an answer to. Send them to the CLI instead.
                    Text("enable with `\(service.connectCommand ?? "ironwire connect")`")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            } else if let command = service.connectCommand {
                Text(command)
                    .font(.caption2.monospaced())
                    .textSelection(.enabled)
                    .foregroundStyle(.secondary)
            }
            Spacer()
        }
        .font(.caption2)
        .buttonStyle(.link)
    }

    /// The consent question, in full, before anything is recorded.
    private func consentQuestion(_ prompt: ConsentPromptView, for service: ServiceView) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(prompt.summary)
                .font(.caption2)
                .fixedSize(horizontal: false, vertical: true)

            // Every point, in the daemon's order. The costs are not moved to
            // the end and not collapsed behind a disclosure.
            ForEach(Array(prompt.points.enumerated()), id: \.offset) { _, point in
                HStack(alignment: .top, spacing: 4) {
                    Text("·")
                    Text(point)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .font(.caption2)
                .foregroundStyle(.secondary)
            }

            Text(prompt.question)
                .font(.caption.weight(.medium))
                .fixedSize(horizontal: false, vertical: true)

            HStack(spacing: 10) {
                // The version that was on screen, not the newest one this build
                // knows about: an answer belongs to the question it answered.
                Button("Enable") {
                    setConsent(service, granted: true, version: prompt.version)
                }
                .disabled(busy)
                Button("Cancel") { asking = nil }
                    .disabled(busy)
                Spacer()
            }
            .font(.caption2)
        }
        .padding(8)
        .background(Color.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 6))
    }

    private func setConsent(_ service: ServiceView, granted: Bool, version: Int) {
        busy = true
        Task {
            error = nil
            let outcome = await client.setConsent(
                backend: service.id, granted: granted, promptVersion: version)
            if case .failure(let failure) = outcome {
                error = failure.message
            } else {
                asking = nil
            }
            busy = false
        }
    }

    // MARK: - Chrome

    private func section<Content: View>(
        _ title: String, @ViewBuilder content: () -> Content
    ) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title.uppercased())
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.secondary)
            content()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
