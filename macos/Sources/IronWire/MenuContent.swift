//! The dropdown.
//
// One pane. A backend is a thing you turn on and then watch, so the switch that
// turns it on and the capacity it reports live on the same row: before consent
// the row invites you to enable it, after consent it shows status. Splitting
// those into a Status tab and a Settings tab meant the answer to "why is nothing
// routing here" was behind a segmented control.
//
// This mirrors `render::status()` in `src/render.rs`, which is the reference for
// what is honest to show — and, more usefully, for what not to. Three of its
// rules are load-bearing here:
//
// - **No bar without an observation.** `Format.capacityFraction` returns `nil`
//   for everything but `observed`, and this file draws nothing when it does.
// - **The privacy line is verbatim.** No shield, no lock, no "protected".
//   `docs/TRUST.md` I7 forbids describing the filter by what the user is safe
//   from, and an icon is a description.
// - **An update is news, never a button.** `docs/UPDATES.md` §1: the daemon
//   holds credentials in the middle of streamed responses and never updates
//   itself. A menu bar app is the most tempting place to break that.

import AppKit
import IronWireKit
import SwiftUI

struct MenuContent: View {
    @ObservedObject var client: ControlClient
    @ObservedObject var notifier: Notifier
    @ObservedObject var loginItem: LoginItem
    @State private var pinError: String?
    @State private var settingsError: String?
    @State private var settingsWarning: String?
    @State private var busy = false
    /// Backends whose consent costs the user has expanded. Not persisted: a
    /// reading aid, not a setting.
    @State private var expanded: Set<String> = []
    /// What the last wiring of each tool reported, keyed by tool id.
    @State private var toolReports: [String: [String]] = [:]
    /// Whether the one-time offer to wire everything has been answered.
    /// Persisted: asking again every launch is how a prompt stops being read.
    @AppStorage("toolsOnboardingAnswered") private var onboardingAnswered = false

    private var iconState: IconState { IconState.from(client.status) }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            if let status = client.status {
                ScrollView {
                    VStack(alignment: .leading, spacing: 14) {
                        onboarding()
                        backendSection(status)
                        toolsSection()
                        usageSection(status.usage)
                        balanceSection(status.balance)
                        routeSection(status)
                        pinSection(status)
                        privacySection(status)
                        updateSection(status.update)
                        if let settingsWarning {
                            Text(settingsWarning).font(.caption).foregroundStyle(.orange)
                        }
                        if let settingsError {
                            Text(settingsError).font(.caption).foregroundStyle(.red)
                        }
                    }
                    .padding(12)
                }
                .frame(maxHeight: 420)
            } else {
                notRunning
            }
            Divider()
            footer
        }
        .frame(width: 340)
        // The only place the poll rate changes. `/_ironwire/status` re-reads a
        // credential per backend, so one second is for when someone is actually
        // watching and five is for the rest of the time.
        .onAppear {
            client.menuIsOpen = true
            // The system is the authority on this, and the user can change it in
            // System Settings while we are not looking.
            loginItem.refresh()
            // The consent prompts and the privacy ladder live here, and this
            // pane cannot offer either without them.
            Task { await client.refreshSettings() }
        }
        .onDisappear { client.menuIsOpen = false }
    }

    // MARK: - Header

    private var header: some View {
        HStack(spacing: 8) {
            Circle()
                .fill(headerColour)
                .frame(width: 8, height: 8)
            VStack(alignment: .leading, spacing: 1) {
                Text(client.status.map { "IronWire \($0.version)" } ?? "IronWire")
                    .font(.headline)
                Text(headerDetail)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
        }
        .padding(12)
    }

    private var headerColour: Color {
        switch iconState {
        case .healthy: return .green
        case .degraded: return .orange
        case .attention: return .red
        case .unreachable: return .secondary
        }
    }

    private var headerDetail: String {
        switch client.connection {
        case .unauthorised:
            return "the control token was rejected"
        case .connecting, .connected, .unreachable:
            return iconState.summary
        }
    }

    // MARK: - Daemon absent

    /// Not an error, and never a dialog. A daemon that is not running is the
    /// state a machine is in most of the time, and this app recovers from it on
    /// its own when one appears.
    private var notRunning: some View {
        VStack(alignment: .leading, spacing: 10) {
            if client.connection == .unauthorised {
                Text("IronWire is running, but rejected this app's token.")
                    .font(.callout)
                Text("The token is read from \(Discovery.home().path)/control.token. Restarting the daemon rewrites it.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                Text("IronWire is not running.")
                    .font(.callout)
                if let binary = Discovery.daemonBinary() {
                    Button("Start IronWire") { start(binary) }
                    Text(binary.path)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                } else {
                    Text("No `ironwire` binary was found. Install it, then run `ironwire serve`.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(12)
    }

    /// Best-effort, and nothing more. Starting a daemon is the user's decision
    /// and the CLI's job; this only saves them a trip to a terminal.
    private func start(_ binary: URL) {
        let process = Process()
        process.executableURL = binary
        process.arguments = ["serve"]
        // Nothing reads these, and an unread pipe that fills would wedge the
        // daemon rather than the app.
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        try? process.run()
    }

    // MARK: - Backends

    /// The settings-side record for a backend, when it has one. The two
    /// documents are joined here rather than in the daemon because they have
    /// different refresh rates: status polls, settings is fetched on open.
    private func service(for id: String) -> ServiceView? {
        client.settings?.services.first { $0.id == id }
    }

    @ViewBuilder
    private func backendSection(_ status: StatusView) -> some View {
        if status.backends.isEmpty {
            section("Backends") {
                Text("None configured. Run `ironwire connect claude`.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        } else {
            section("Backends") {
                ForEach(status.backends) { backend in
                    backendRow(backend)
                }
            }
        }
    }

    private func backendRow(_ backend: BackendView) -> some View {
        let service = service(for: backend.id)
        let prompt = service?.consentPrompt
        let switchable = service?.canToggle == true && prompt?.isComplete == true

        return VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                Text(backend.name).font(.callout.weight(.medium))
                Spacer()
                if switchable, let service, let prompt {
                    consentSwitch(service, prompt: prompt)
                } else {
                    Text(Format.kind(backend.kind))
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }

            if !backend.authenticated {
                // A credential this app cannot conjure. It comes from the user
                // logging into Claude Code or Codex, or exporting an API key, so
                // the row names that command rather than offering a login.
                Text(backend.detail ?? "not connected")
                    .font(.caption)
                    .foregroundStyle(.red)
                if let command = service?.connectCommand {
                    Text(command)
                        .font(.caption2.monospaced())
                        .textSelection(.enabled)
                        .foregroundStyle(.secondary)
                }
            } else if !backend.consented {
                invitation(backend, service: service, prompt: prompt, switchable: switchable)
            } else {
                capacity(backend.headroom)
                if let health = Format.healthSummary(backend.health) {
                    Text(health)
                        .font(.caption)
                        .foregroundStyle(backend.health.isOpen ? .red : .orange)
                }
            }
        }
    }

    /// What a backend says before it is enabled.
    ///
    /// The switch is on the row above. The row itself stays to two short lines —
    /// a menu is read at a glance, and paragraphs per backend is what made the
    /// old pane unreadable — so the daemon's summary and its points both live
    /// behind **"What you are taking on"**, one click above the switch. Neither
    /// is ever reworded, reordered, or abridged; what changed is that they are a
    /// click away rather than always drawn, and `docs/TRUST.md` §2 records what
    /// that costs. Where there is no usable prompt the row names the command
    /// instead — a switch is not offered for a question this build could not
    /// read.
    @ViewBuilder
    private func invitation(
        _ backend: BackendView, service: ServiceView?, prompt: ConsentPromptView?, switchable: Bool
    ) -> some View {
        if switchable, let prompt {
            Text("Not enabled — turn it on to route here.")
                .font(.caption)
                .foregroundStyle(.orange)

            Button {
                if expanded.contains(backend.id) {
                    expanded.remove(backend.id)
                } else {
                    expanded.insert(backend.id)
                }
            } label: {
                HStack(spacing: 3) {
                    Image(systemName: expanded.contains(backend.id) ? "chevron.down" : "chevron.right")
                        .font(.system(size: 8))
                    Text("What you are taking on")
                }
                .font(.caption2)
            }
            .buttonStyle(.link)
            .accessibilityLabel(Text("What you are taking on, \(prompt.points.count) points"))

            if expanded.contains(backend.id) {
                Text(prompt.summary)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                // Every point, in the daemon's order. The costs are not moved to
                // the end and not summarised.
                ForEach(Array(prompt.points.enumerated()), id: \.offset) { _, point in
                    HStack(alignment: .top, spacing: 4) {
                        Text("·")
                        Text(point)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                }
            }
        } else {
            Text("not enabled")
                .font(.caption)
                .foregroundStyle(.orange)
            // The daemon's own field, verbatim. Composing one here — appending a
            // flag, substituting a backend name — is how the old pane came to
            // tell every backend to run `ironwire connect claude`.
            if let command = service?.connectCommand {
                Text(command)
                    .font(.caption2.monospaced())
                    .textSelection(.enabled)
                    .foregroundStyle(.secondary)
            }
        }
    }

    /// The switch that grants or revokes consent.
    ///
    /// One flip is the whole action. The version travels with it — the one that
    /// was on screen, not the newest this build knows about, because an answer
    /// belongs to the question it answered.
    private func consentSwitch(_ service: ServiceView, prompt: ConsentPromptView) -> some View {
        Toggle("", isOn: Binding(
            get: { service.consented },
            set: { wanted in
                setConsent(service, granted: wanted, version: wanted ? prompt.version : 0)
            }
        ))
        .labelsHidden()
        .toggleStyle(.switch)
        .controlSize(.mini)
        .disabled(busy)
        .accessibilityLabel(Text("Enable \(service.name)"))
        .accessibilityHint(Text(prompt.summary))
    }

    private func setConsent(_ service: ServiceView, granted: Bool, version: Int) {
        busy = true
        Task {
            settingsError = nil
            let outcome = await client.setConsent(
                backend: service.id, granted: granted, promptVersion: version)
            if case .failure(let failure) = outcome {
                settingsError = failure.message
            }
            busy = false
        }
    }

    /// The bar, or the absence of one.
    ///
    /// `capacityFraction` is `nil` for every state but `observed`, and this is
    /// the branch that honours it. A `ProgressView` needs a `Double` and there
    /// is no honest one to give it here — 0 reads as empty, a grey half-bar
    /// reads as half, and both are numbers nobody measured.
    @ViewBuilder
    private func capacity(_ headroom: HeadroomView) -> some View {
        if let fraction = Format.capacityFraction(headroom) {
            ProgressView(value: fraction)
                .progressViewStyle(.linear)
                .tint(colour(for: Format.usageLevel(usedPct: fraction * 100)))
        }
        Text(Format.headroomSummary(headroom))
            .font(.caption)
            .foregroundStyle(.secondary)
    }

    private func colour(for level: Format.UsageLevel) -> Color {
        switch level {
        case .good: return .green
        case .warn: return .orange
        case .bad: return .red
        }
    }

    // MARK: - Onboarding

    /// Offer, once, to point every agent on this machine at IronWire.
    ///
    /// A daemon nothing routes through is the state a fresh install is in, and
    /// the one a user is least equipped to diagnose: every backend healthy,
    /// capacity plentiful, and their agent talking straight to the provider.
    /// Asking once is the whole of the interaction — after that the switches in
    /// the Tools section are there, and a banner that keeps coming back is one
    /// people learn to dismiss without reading.
    ///
    /// Not a sheet. A modal over a menu bar panel is a dialog that blocks the
    /// one surface the user opened to get an answer, and the answer here fits
    /// in two lines.
    @ViewBuilder
    private func onboarding() -> some View {
        let candidates = (client.settings?.tools ?? []).filter { $0.installed && !$0.wired }
        if !onboardingAnswered, !candidates.isEmpty {
            VStack(alignment: .leading, spacing: 6) {
                Text("\(Format.list(candidates.map(\.name))) not routed through IronWire.")
                    .font(.caption)
                    .fixedSize(horizontal: false, vertical: true)
                HStack(spacing: 10) {
                    Button("Point \(candidates.count == 1 ? "it" : "them") here") {
                        onboardingAnswered = true
                        for tool in candidates {
                            setTool(tool, connect: true)
                        }
                    }
                    .disabled(busy)
                    Button("Not now") { onboardingAnswered = true }
                    Spacer()
                }
                .font(.caption2)
                .buttonStyle(.link)
            }
            .padding(8)
            .background(Color.secondary.opacity(0.10), in: RoundedRectangle(cornerRadius: 6))
        }
    }

    // MARK: - Tools

    /// Which agents on this machine actually send their traffic here.
    ///
    /// The one thing the rest of this pane cannot tell you. Every backend can
    /// be healthy, a subscription consented and capacity plentiful, and still
    /// nothing routes — because the agent was never pointed at IronWire and
    /// went straight to the provider instead. A green dot beside a silent
    /// proxy is the most misleading state this app can show, and this section
    /// exists to make it impossible.
    ///
    /// Only tools that are actually here are listed. The daemon reports the
    /// others so a client *can* say "never heard of it", but a dropdown listing
    /// editors you do not have is noise.
    @ViewBuilder
    private func toolsSection() -> some View {
        let tools = (client.settings?.tools ?? []).filter(\.installed)
        if !tools.isEmpty {
            section("Tools") {
                ForEach(tools) { tool in
                    VStack(alignment: .leading, spacing: 1) {
                        HStack(spacing: 6) {
                            Text(tool.name).font(.caption)
                            Spacer()
                            Toggle("", isOn: Binding(
                                get: { tool.wired },
                                set: { wanted in setTool(tool, connect: wanted) }
                            ))
                            .labelsHidden()
                            .toggleStyle(.switch)
                            .controlSize(.mini)
                            .disabled(busy)
                            .accessibilityLabel(Text("Route \(tool.name) through IronWire"))
                            .accessibilityHint(Text(tool.configPath ?? tool.connectCommand))
                        }
                        if !tool.wired {
                            Text("not routed here")
                                .font(.caption2)
                                .foregroundStyle(.orange)
                        }
                        // What the last edit did, and to which file. This is
                        // the one write that changes something outside
                        // `$IRONWIRE_HOME`, and every other surface in this
                        // product names the file before touching it.
                        if let report = toolReports[tool.id] {
                            ForEach(Array(report.enumerated()), id: \.offset) { _, line in
                                Text(line)
                                    .font(.caption2)
                                    .foregroundStyle(.secondary)
                                    .fixedSize(horizontal: false, vertical: true)
                            }
                        }
                    }
                    .help(tool.configPath ?? "")
                }
            }
        }
    }

    /// Wire a tool, and keep what the daemon said it did.
    ///
    /// The report is shown rather than discarded because a slot the user was
    /// already using is left alone — a switch that flicks back with no
    /// explanation is the worst version of that, and the daemon has the
    /// sentence that explains it.
    private func setTool(_ tool: ToolView, connect: Bool) {
        busy = true
        Task {
            settingsError = nil
            switch await client.setTool(id: tool.id, connect: connect) {
            case .success(let outcome):
                var lines = outcome.changes
                lines.append(contentsOf: outcome.occupied)
                if let path = outcome.path, !lines.isEmpty {
                    lines.append(path)
                }
                toolReports[tool.id] = lines
            case .failure(let failure):
                settingsError = failure.message
            }
            busy = false
        }
    }

    // MARK: - Usage

    /// What IronWire itself has sent this window.
    ///
    /// This is the one block on the screen that shows a percentage without the
    /// provider having reported anything, and it is allowed to because it is
    /// not a claim about the provider (`AGENTS.md` rule 2, the `ironwire_usage`
    /// exception). The guard is that a percentage is never drawn without
    /// `ceiling.describes` next to it saying what it is a percentage *of* —
    /// "your own p90 over 14 sessions" is a different sentence from "42% of
    /// your plan", and only the first one is true.
    @ViewBuilder
    private func usageSection(_ usage: UsageView) -> some View {
        if !usage.sessions.isEmpty {
            section("This \(usage.sessionHours)h window") {
                ForEach(usage.sessions) { session in
                    VStack(alignment: .leading, spacing: 1) {
                        HStack(spacing: 6) {
                            Text(session.backend).font(.caption)
                            Spacer()
                            if let pct = session.usedPct {
                                Text("\(Int(pct.rounded()))%")
                                    .font(.caption)
                                    .foregroundStyle(colour(for: Format.usageLevel(usedPct: pct)))
                            } else {
                                // No ceiling to be a percentage of. The
                                // exchange count is a fact; a percentage here
                                // would be one we made up.
                                Text("\(session.exchanges) exchange(s)")
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                        }
                        if let ceiling = session.ceiling {
                            Text(ceiling.unverified
                                ? "of \(ceiling.describes) (unverified)"
                                : "of \(ceiling.describes)")
                                .font(.caption2)
                                .foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                        // The question the block exists to answer, and only
                        // said when the daemon has the rate to answer it.
                        if session.exhaustsBeforeClose, let minutes = session.exhaustsInMinutes {
                            Text("at this rate, reached in \(Format.duration(Int(minutes * 60)))")
                                .font(.caption2)
                                .foregroundStyle(.orange)
                        }
                    }
                }
            }
        }
    }

    // MARK: - Balance

    @ViewBuilder
    private func balanceSection(_ balance: BalanceView) -> some View {
        section("Balance") {
            Text(poolLine(balance))
                .font(.caption)

            ForEach(balance.subscriptionUsed) { use in
                HStack {
                    Text(use.name).font(.caption)
                    Spacer()
                    // `nil` is "the provider has not said", and it says so.
                    // Filling it in would be the same fabrication as a bar
                    // drawn at zero.
                    if let pct = use.usedPct {
                        Text("\(Int(pct.rounded()))% used")
                            .font(.caption)
                            .foregroundStyle(colour(for: Format.usageLevel(usedPct: pct)))
                    } else {
                        Text("not yet reported")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            }

            if let cap = balance.spendCap {
                Text("spend today: \(Format.currency(cap.spentUsd)) of \(Format.currency(cap.capUsd)) cap")
                    .font(.caption)
            }
            // Zero is a result, not an absence: it is the sentence "nothing was
            // billed today", which is the point of routing to a subscription.
            if let spend = balance.spendTodayUsd {
                Text("metered spend today: \(Format.currency(spend))")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                Text("metered spend today: not recorded (ledger off)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private func poolLine(_ balance: BalanceView) -> String {
        var parts: [String] = []
        if balance.available > 0 {
            switch balance.freeAvailable {
            case 0:
                parts.append("\(balance.available) pool(s) available")
            case balance.available:
                parts.append("\(balance.available) pool(s) available, all already paid for")
            default:
                parts.append("\(balance.available) pool(s) available (\(balance.freeAvailable) already paid for)")
            }
        }
        // Never folded into "available": the provider has told us nothing, and
        // reporting that as headroom is the fabrication this screen avoids.
        if balance.unknown > 0 { parts.append("\(balance.unknown) not yet reporting") }
        if balance.unavailable > 0 {
            if let at = balance.nextAvailableAt {
                let secs = Int(at.timeIntervalSinceNow)
                parts.append("\(balance.unavailable) unavailable · first back in \(Format.duration(secs))")
            } else {
                parts.append("\(balance.unavailable) unavailable")
            }
        }
        return parts.isEmpty ? "no pools yet" : parts.joined(separator: " · ")
    }

    // MARK: - Route

    @ViewBuilder
    private func routeSection(_ status: StatusView) -> some View {
        if let route = status.lastRoute {
            section("Current route") {
                Text(Format.routeSummary(route))
                    .font(.callout.weight(.medium))
                HStack(spacing: 4) {
                    if let model = route.model {
                        Text(model).font(.caption)
                    }
                    Text("· \(Format.relative(route.at)) ago")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                // The rung comes off the wire. Working "is this degraded" out
                // from backend names here would be a second implementation of a
                // routing question, in a language that cannot see the policy.
                if route.rung.isDegraded {
                    Text(route.rung.label)
                        .font(.caption)
                        .foregroundStyle(route.rung.isUserVisible ? .red : .orange)
                }
            }
        }
    }

    // MARK: - Pin

    private func pinSection(_ status: StatusView) -> some View {
        section("Route to") {
            Menu(status.pin ?? "Automatic") {
                Button("Automatic") { setPin(nil) }
                Divider()
                ForEach(status.backends) { backend in
                    Button(backend.id) { setPin(backend.id) }
                }
            }
            .menuStyle(.borderlessButton)
            .fixedSize()

            if let pinError {
                Text(pinError)
                    .font(.caption)
                    .foregroundStyle(.red)
            }
        }
    }

    private func setPin(_ backend: String?) {
        Task {
            pinError = nil
            // The daemon validates and stores; the menu shows whatever the next
            // poll reports, not what we asked for.
            if case .failure(let error) = await client.pin(backend: backend) {
                pinError = error.message
            }
        }
    }

    // MARK: - Privacy

    /// The filter, as a control when the daemon has told us what is selectable
    /// and as its own words when it has not.
    ///
    /// Either way this says what the filter is *doing*, never what the user is
    /// safe from: no shield, no lock, no "protected" (`docs/TRUST.md` I7).
    @ViewBuilder
    private func privacySection(_ status: StatusView) -> some View {
        if let privacy = client.settings?.privacy, !privacy.options.isEmpty {
            section("Privacy filter") {
                // Nothing but the control. What each mode substitutes, and why a
                // dimmed one cannot be picked, are both carried as the segment's
                // tooltip and its VoiceOver hint — reachable, and not four lines
                // of prose in a dropdown read at a glance.
                //
                // This is weaker than drawing the reason: a user who never
                // hovers sees a mode they cannot select and no explanation. The
                // stronger version is one `ForEach` in `privacySection`. It is
                // also mostly moot once a trusted backend ships by default,
                // because then `full` is selectable and there is no reason to
                // show.
                privacyControl(privacy)
            }
        } else if let privacy = status.privacy {
            section("Privacy filter") {
                Text(privacy)
                    .font(.caption)
                    .textSelection(.enabled)
            }
        }
    }

    /// The ladder as one control.
    ///
    /// Hand-rolled rather than a segmented `Picker` because `Picker` cannot
    /// disable an individual segment, and `full` being unselectable — with its
    /// reason — is the one thing this control has to get right.
    private func privacyControl(_ privacy: PrivacySettingsView) -> some View {
        HStack(spacing: 2) {
            ForEach(privacy.options) { option in
                let isCurrent = option.id == privacy.mode
                Button {
                    apply(mode: option.id)
                } label: {
                    Text(option.id)
                        .font(.caption2.weight(isCurrent ? .semibold : .regular))
                        .lineLimit(1)
                        .minimumScaleFactor(0.8)
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 4)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .background(
                    RoundedRectangle(cornerRadius: 5)
                        .fill(isCurrent ? Color.accentColor.opacity(0.30) : Color.clear)
                )
                .disabled(!option.selectable || busy || isCurrent)
                .opacity(option.selectable ? 1 : 0.45)
                .help(option.describes)
                // Without this the segments are four unlabelled buttons to
                // VoiceOver, which is how the mode choice becomes unusable.
                .accessibilityLabel(Text(option.id))
                .accessibilityHint(Text(option.selectable ? option.describes : (option.unavailableBecause ?? option.describes)))
                .accessibilityAddTraits(isCurrent ? [.isSelected] : [])
            }
        }
        .padding(2)
        .background(RoundedRectangle(cornerRadius: 7).fill(Color.secondary.opacity(0.12)))
    }

    private func apply(mode: String) {
        busy = true
        Task {
            settingsError = nil
            settingsWarning = nil
            switch await client.setPrivacyMode(mode) {
            case .success(let outcome):
                // Applied, but not saved. Said plainly rather than swallowed:
                // the user would otherwise find the old mode back after a
                // restart with no idea why.
                settingsWarning = outcome.warning
            case .failure(let failure):
                settingsError = failure.message
            }
            busy = false
        }
    }

    // MARK: - Update

    @ViewBuilder
    private func updateSection(_ update: UpdateStatus) -> some View {
        switch update {
        case .available(let latest, let summary, let command):
            section("Update") {
                // A link, and nothing that could apply it. See the file header.
                Link("ironwire \(latest) is available", destination: releasesURL)
                    .font(.caption)
                if let summary {
                    Text(summary).font(.caption).foregroundStyle(.secondary)
                }
                if let command {
                    Text(command)
                        .font(.caption.monospaced())
                        .textSelection(.enabled)
                        .foregroundStyle(.secondary)
                }
            }
        case .unsupported(let latest, let minimum, let command):
            section("Update") {
                Text("This build is below the supported floor (\(minimum)); providers may have changed in ways it does not handle.")
                    .font(.caption)
                    .foregroundStyle(.red)
                Link("ironwire \(latest) is available", destination: releasesURL)
                    .font(.caption)
                if let command {
                    Text(command)
                        .font(.caption.monospaced())
                        .textSelection(.enabled)
                        .foregroundStyle(.secondary)
                }
            }
        // `unrecognised` is a state from a newer daemon. Showing nothing is the
        // only honest option — the alternative is guessing what it meant.
        case .upToDate, .unknown, .unrecognised:
            EmptyView()
        }
    }

    private var releasesURL: URL {
        URL(string: "https://github.com/nearai/ironwire/releases") ?? URL(fileURLWithPath: "/")
    }

    // MARK: - Footer

    private var footer: some View {
        VStack(alignment: .leading, spacing: 8) {
            Toggle("Notify on family changes and failures", isOn: $notifier.enabled)
                .font(.caption)
                .toggleStyle(.checkbox)

            Toggle("Open at login", isOn: Binding(
                get: { loginItem.isOn },
                set: { loginItem.set($0) }
            ))
            .font(.caption)
            .toggleStyle(.checkbox)
            if let detail = loginItem.detail {
                Text(detail)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }

            HStack(spacing: 12) {
                if let status = client.status {
                    Button("Copy control URL") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(
                            "http://127.0.0.1:\(status.port)", forType: .string
                        )
                    }
                }
                // Only offered when there is something to open. A menu item that
                // opens nothing is worse than one that is absent — a foreground
                // `ironwire serve` logs to its own terminal and has no file.
                if let log = Discovery.brewLog() {
                    Button("Open log") { NSWorkspace.shared.open(log) }
                } else {
                    Button("Reveal ~/.ironwire") {
                        NSWorkspace.shared.selectFile(nil, inFileViewerRootedAtPath: Discovery.home().path)
                    }
                }
                Spacer()
                Button("Quit") { NSApp.terminate(nil) }
            }
            .font(.caption)
            .buttonStyle(.link)
        }
        .padding(12)
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
