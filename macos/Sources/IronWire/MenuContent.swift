//! The dropdown.
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
// - **An update is news, never a button.** `docs/UPDATES.md` §1: IronWire holds
//   credentials in the middle of streamed responses and never updates itself. A
//   menu bar app is the most tempting place in the product to break that.

import AppKit
import IronWireKit
import SwiftUI

struct MenuContent: View {
    @ObservedObject var client: ControlClient
    @ObservedObject var notifier: Notifier
    @ObservedObject var loginItem: LoginItem
    @State private var pinError: String?
    @State private var pane: Pane = .status

    /// The two halves of the menu. Settings is a separate pane rather than more
    /// rows: the status pane is read at a glance under pressure, and burying
    /// what it says under a consent question would be the wrong trade.
    enum Pane: String, CaseIterable, Identifiable {
        case status = "Status"
        case settings = "Settings"
        var id: String { rawValue }
    }

    private var iconState: IconState { IconState.from(client.status) }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            if client.status != nil {
                Picker("", selection: $pane) {
                    ForEach(Pane.allCases) { Text($0.rawValue).tag($0) }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .padding(.horizontal, 12)
                .padding(.top, 8)
            }
            if let status = client.status {
                ScrollView {
                    switch pane {
                    case .status:
                        VStack(alignment: .leading, spacing: 14) {
                            backendSection(status)
                            balanceSection(status.balance)
                            routeSection(status)
                            pinSection(status)
                            privacySection(status)
                            updateSection(status.update)
                        }
                        .padding(12)
                    case .settings:
                        SettingsContent(client: client)
                    }
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
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                Text(backend.name).font(.callout.weight(.medium))
                Spacer()
                Text(Format.kind(backend.kind))
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }

            if !backend.authenticated {
                Text(backend.detail ?? "not connected")
                    .font(.caption)
                    .foregroundStyle(.red)
            } else if !backend.consented {
                Text("awaiting consent — run `ironwire connect claude --subscription`")
                    .font(.caption)
                    .foregroundStyle(.orange)
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

    @ViewBuilder
    private func privacySection(_ status: StatusView) -> some View {
        if let privacy = status.privacy {
            section("Privacy filter") {
                // Verbatim, and only ever verbatim. The daemon says what the
                // filter is *doing*; anything this app added would be a claim
                // about what the user is safe from (`docs/TRUST.md` I7).
                Text(privacy)
                    .font(.caption)
                    .textSelection(.enabled)
            }
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
