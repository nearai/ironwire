//! Swift mirrors of `GET /_ironwire/settings`.
//
// The settings screen is where a GUI is most tempted to grow its own opinion:
// four privacy modes look like four buttons, and "log in" looks like something
// an app should just do. Both would be wrong.
//
// So this type carries the *decisions* as well as the values. Whether `full` is
// selectable depends on `trusted_backends`, a rule that lives in
// `Config::validate`; the app reads `selectable` and `unavailableBecause` rather
// than working it out. The consent question arrives as text from the daemon,
// because a second copy of a consent prompt is a second prompt while the
// recorded version claims otherwise (`docs/TRUST.md` §2).

import Foundation

/// What can be changed, and what it would take.
public struct SettingsView: Decodable, Sendable, Equatable {
    /// The privacy filter, and the modes it could be switched to.
    public let privacy: PrivacySettingsView
    /// Everything a user can log into.
    public let services: [ServiceView]

    private enum CodingKeys: String, CodingKey {
        case privacy, services
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        privacy = try c.decodeIfPresent(PrivacySettingsView.self, forKey: .privacy)
            ?? PrivacySettingsView()
        services = try c.decodeIfPresent([ServiceView].self, forKey: .services) ?? []
    }

    public init(privacy: PrivacySettingsView = PrivacySettingsView(), services: [ServiceView] = []) {
        self.privacy = privacy
        self.services = services
    }
}

/// The privacy filter as a settings screen sees it.
public struct PrivacySettingsView: Decodable, Sendable, Equatable {
    /// The mode in force: `off` / `credentials` / `pii` / `full`.
    public let mode: String
    /// What the filter is *doing*, in the daemon's words. Rendered verbatim or
    /// not at all (`docs/TRUST.md` I7).
    public let summary: String
    /// Every rung of the ladder, in order.
    public let options: [PrivacyOptionView]
    /// Backends named as acceptable destinations under `full`.
    public let trustedBackends: [String]

    private enum CodingKeys: String, CodingKey {
        case mode, summary, options, trustedBackends
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        mode = try c.decodeIfPresent(String.self, forKey: .mode) ?? "off"
        summary = try c.decodeIfPresent(String.self, forKey: .summary) ?? "off"
        options = try c.decodeIfPresent([PrivacyOptionView].self, forKey: .options) ?? []
        trustedBackends = try c.decodeIfPresent([String].self, forKey: .trustedBackends) ?? []
    }

    public init(
        mode: String = "off", summary: String = "off",
        options: [PrivacyOptionView] = [], trustedBackends: [String] = []
    ) {
        self.mode = mode
        self.summary = summary
        self.options = options
        self.trustedBackends = trustedBackends
    }
}

/// One rung of the privacy ladder.
public struct PrivacyOptionView: Decodable, Sendable, Equatable, Identifiable {
    /// The value to send back. Also the identity, so a mode this build has never
    /// heard of still renders as itself.
    public let id: String
    /// What this level substitutes, in one clause.
    public let describes: String
    /// Whether switching to it right now would work.
    public let selectable: Bool
    /// Why it would not. Shown beside the disabled option, because an option
    /// that is greyed out for unstated reasons is worse than one that is absent.
    public let unavailableBecause: String?

    private enum CodingKeys: String, CodingKey {
        case id, describes, selectable, unavailableBecause
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decodeIfPresent(String.self, forKey: .id) ?? ""
        describes = try c.decodeIfPresent(String.self, forKey: .describes) ?? ""
        // Absent means selectable: an older daemon that does not send the field
        // has no restrictions to report.
        selectable = try c.decodeIfPresent(Bool.self, forKey: .selectable) ?? true
        unavailableBecause = try c.decodeIfPresent(String.self, forKey: .unavailableBecause)
    }

    public init(id: String, describes: String, selectable: Bool = true, unavailableBecause: String? = nil) {
        self.id = id
        self.describes = describes
        self.selectable = selectable
        self.unavailableBecause = unavailableBecause
    }
}

/// One thing a user can log into.
public struct ServiceView: Decodable, Sendable, Equatable, Identifiable {
    /// Backend id.
    public let id: String
    /// Display name.
    public let name: String
    /// `subscription` / `api_key` / `credits` / `local`.
    public let kind: String
    /// Whether a credential was found.
    public let authenticated: Bool
    /// Why not, when it was not.
    public let detail: String?
    /// Whether this backend is gated behind recorded consent.
    public let requiresConsent: Bool
    /// Whether that consent is recorded, at the current prompt version.
    public let consented: Bool
    /// The exact question that has to be answered to enable it.
    public let consentPrompt: ConsentPromptView?
    /// What to run for the part a GUI has no business doing.
    public let connectCommand: String?

    private enum CodingKeys: String, CodingKey {
        case id, name, kind, authenticated, detail
        case requiresConsent, consented, consentPrompt, connectCommand
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decodeIfPresent(String.self, forKey: .id) ?? ""
        name = try c.decodeIfPresent(String.self, forKey: .name) ?? id
        kind = try c.decodeIfPresent(String.self, forKey: .kind) ?? ""
        authenticated = try c.decodeIfPresent(Bool.self, forKey: .authenticated) ?? false
        detail = try c.decodeIfPresent(String.self, forKey: .detail)
        requiresConsent = try c.decodeIfPresent(Bool.self, forKey: .requiresConsent) ?? false
        consented = try c.decodeIfPresent(Bool.self, forKey: .consented) ?? false
        consentPrompt = try c.decodeIfPresent(ConsentPromptView.self, forKey: .consentPrompt)
        connectCommand = try c.decodeIfPresent(String.self, forKey: .connectCommand)
    }

    public init(
        id: String, name: String, kind: String = "subscription", authenticated: Bool = true,
        detail: String? = nil, requiresConsent: Bool = false, consented: Bool = true,
        consentPrompt: ConsentPromptView? = nil, connectCommand: String? = nil
    ) {
        self.id = id
        self.name = name
        self.kind = kind
        self.authenticated = authenticated
        self.detail = detail
        self.requiresConsent = requiresConsent
        self.consented = consented
        self.consentPrompt = consentPrompt
        self.connectCommand = connectCommand
    }

    /// Whether this is a switch the app can actually throw.
    ///
    /// Consent is the only thing here the daemon can act on directly. A
    /// credential that has not been found is not something a menu can conjure —
    /// it comes from the user logging into Claude Code or Codex, or exporting an
    /// API key, and the honest thing is to say so and name the command.
    public var canToggle: Bool { requiresConsent && authenticated }
}

/// The consent question, exactly as the daemon words it.
///
/// Never composed here, never abridged, never reordered so the cost reads last.
/// This is the wording `CONSENT_PROMPT_VERSION` is recorded against, and the
/// version travels with it so the answer can be checked against the question.
public struct ConsentPromptView: Decodable, Sendable, Equatable {
    /// The version this wording is. Sent back with the answer.
    public let version: Int
    /// Backend this grants.
    public let backendId: String
    /// What the user calls it.
    public let product: String
    /// What IronWire will do, in one sentence.
    public let summary: String
    /// What the user is taking on, one point at a time.
    public let points: [String]
    /// The question itself.
    public let question: String

    private enum CodingKeys: String, CodingKey {
        case version, backendId, product, summary, points, question
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        version = try c.decodeIfPresent(Int.self, forKey: .version) ?? 0
        backendId = try c.decodeIfPresent(String.self, forKey: .backendId) ?? ""
        product = try c.decodeIfPresent(String.self, forKey: .product) ?? ""
        summary = try c.decodeIfPresent(String.self, forKey: .summary) ?? ""
        points = try c.decodeIfPresent([String].self, forKey: .points) ?? []
        question = try c.decodeIfPresent(String.self, forKey: .question) ?? ""
    }

    public init(
        version: Int, backendId: String, product: String,
        summary: String, points: [String], question: String
    ) {
        self.version = version
        self.backendId = backendId
        self.product = product
        self.summary = summary
        self.points = points
        self.question = question
    }

    /// Whether this is safe to present at all.
    ///
    /// A prompt that arrived without its summary or its points is not a consent
    /// screen — it is a button with a title. Better to send the user to the CLI
    /// than to collect an answer to a question we failed to ask.
    public var isComplete: Bool {
        version > 0 && !summary.isEmpty && !points.isEmpty && !question.isEmpty
    }
}
