# IronWire menu bar app

A macOS `MenuBarExtra` that shows what the daemon is doing, and lets you pin
where traffic goes. It is a **pure client** of the control API
(`docs/DESIGN.md` §6): it renders `/_ironwire/status` and posts to
`/_ironwire/pin`, and it decides nothing.

```
        ┌──────────────────────┐
        │  IronWire.app        │
        │  (SwiftUI, no logic) │
        └──────────┬───────────┘
          poll 5s  │  SSE (held open)
                   ▼
        GET  /_ironwire/status     ── StatusView
        GET  /_ironwire/events     ── Event stream
        POST /_ironwire/pin        ── the only write
                   │  Bearer $IRONWIRE_HOME/control.token
                   ▼
        ┌──────────────────────┐
        │  ironwire serve      │  ← every decision happens here
        └──────────────────────┘
```

## Build and run

```bash
make -C macos test     # the Swift suite
make -C macos app      # build/IronWire.app — universal, ad-hoc signed
make -C macos run      # build it and launch it
make -C macos dist     # build/IronWire-macos.zip, the release artifact
```

Needs Xcode's toolchain (`xcodebuild -version` ≥ 15) and macOS 13 or newer.
`ARCHS=` skips the universal build for a faster local loop:

```bash
make -C macos app ARCHS=
```

Xcode opens `Package.swift` directly if you would rather work there.

## The rule

**No routing logic in Swift.** Any conditional here that would have to change
when routing policy changes is a bug. If the app needs a derived value, the
field goes into `StatusView` and is computed in Rust — because two
implementations of "is this backend usable" that drift is the specific failure
a GUI over a daemon invites.

The concrete form of that rule is `Format.capacityFraction`, which returns
`nil` for every headroom state but `observed`. `ProgressView` wants a `Double`
and there isn't an honest one: zero reads as empty, a grey half-bar reads as
half, and both are numbers IronWire made up in the one surface a user takes in
without reading. So an unobserved backend gets the word `unknown` and **no bar**
(`AGENTS.md` rule 2, `docs/CRITIQUE.md` §4).

`Format` also mirrors the CLI's arithmetic — the ten-cell meter from
`render::meter()` and the 50/90 colour thresholds from `Style::by_usage` — so a
screenshot of this and a screenshot of `ironwire status` cannot disagree about
what "nearly full" looks like.

## Layout

```
Package.swift
Makefile                       app / dist / run / test / clean
Resources/Info.plist           LSUIElement, bundle id, version
Sources/IronWireKit/           everything decidable without a window
  Models.swift                 Codable mirrors of StatusView / Event
  ControlClient.swift          token + port discovery, polling, SSE, pin
  Discovery.swift              $IRONWIRE_HOME, control.token, config.toml
  Format.swift                 the bar, the meter, the wording
  IconState.swift              what the icon says
  SSE.swift                    comment frames vs data frames
  NotificationPolicy.swift     which events are worth interrupting someone for
Sources/IronWire/              views and wiring only
  IronWireApp.swift            the MenuBarExtra scene
  MenuContent.swift            the dropdown
  MenuBarIcon.swift            drawing the four icon states
  Notifications.swift          opt-in UNUserNotificationCenter
Tests/
```

The split is what makes the rules testable: every decision lives in
`IronWireKit` and is covered by `swift test`, and the app target is left with
nothing but layout.

## Why SwiftPM and not an `.xcodeproj`

Issue #8 asks for `IronWire.xcodeproj`. This is a package instead, for two
reasons. The release job has to build the bundle from a script, so a
terminal-driven build is required either way — and a hand-authored `.pbxproj` is
several hundred lines of generated-looking UUIDs that no reviewer reads and
nobody hand-edits. `Package.swift` is a file you can read in one screen, Xcode
opens it directly, and `make app` produces the same bundle locally that CI
produces for a release.

## Decisions worth knowing about

**The App Sandbox is off.** The app reads `control.token` (mode 0600) from
`$IRONWIRE_HOME`, and a sandboxed app cannot see `~/.ironwire` without a
user-selected read entitlement — which means a file picker on first launch, for
a file the user has no reason to know about. The alternative, copying the token
somewhere the sandbox can reach, would mean a second copy of a credential that
grants control over where someone's traffic goes. Neither is worth it for a
local tool that talks only to `127.0.0.1`. If this ever ships through the App
Store, that decision has to be revisited, because the sandbox is mandatory there.

**The port is found with a line scan, not a TOML parser.** `Discovery.port`
looks for `port = N` under `[server]` in `config.toml` and falls back to 8463.
A TOML dependency is not worth one integer, and getting it wrong costs a
"not running" message — a state the app already handles — rather than anything
silent. A `port` under any other table is ignored, which is the only mistake a
naive search would actually make.

**The dropdown is `.window`, not `.menu`.** A real menu cannot express "no bar
here" as clearly as a view can, and `.window` gives `onAppear`/`onDisappear`,
which is how the poll rate switches between 5s closed and 1s open.
`/_ironwire/status` does a credential check per backend — a Keychain read for
the Claude one — so a one-second background poll would not be free.

**Notifications are off until asked for**, and authorisation is requested at the
moment they are switched on rather than at first launch. Only the events the
daemon itself considers user-visible produce one: a cross-family route, a
failure, and a spend cap being reached (`Event::is_user_visible` in
`crates/ironwire_proxy/src/events.rs`). A notification per route change would be
unusable, and the bus is deliberately lossy — this app is too.

**No install button, ever.** An available update is shown as a link and the
upgrade command for the user's install. IronWire never updates itself: it holds
credentials in the middle of streamed responses, and a restart mid-turn causes
exactly the outage the product exists to prevent (`docs/UPDATES.md` §1). A menu
bar app is the most tempting place in the codebase to break that.

**The privacy line is verbatim.** Whatever `status.privacy` says is what is
shown. No shield, no lock, no "protected" — `docs/TRUST.md` I7 forbids
describing the filter by what the user is safe from, and an icon is a
description.

## Testing against a real daemon

The unit suite covers the rules and the decoding. A handful of checks need an
actual daemon and are skipped without one:

```bash
ironwire serve &
IRONWIRE_LIVE=1 swift test --filter LiveDaemonTests
```

They cover the parts only a real server can show: that the document the daemon
emits is the one this app expects, that a 401 re-reads the token file and
retries once, and that a pin round-trips. One of them changes the daemon's pin
and clears it again, which is why they are opt-in.

For the states a real daemon will not produce on request — an open circuit, a
cross-family route, an unobserved backend, a tag from a newer daemon — the
dropdown is rendered off-screen with `ImageRenderer` in `MenuContentTests`.

## CI and distribution

`make -C macos test` runs on the macOS leg of CI only; the Linux legs cannot
build Swift and a red matrix on every push would be worse than no coverage.

The release job builds `IronWire-macos.zip` on the macOS runner and the Homebrew
formula installs it on macOS. The bundle is **ad-hoc signed**, which is enough to
run locally and enough to execute on Apple Silicon, but *not* enough for a bundle
that arrives from the internet: Gatekeeper wants notarisation, which needs an
Apple Developer certificate this project does not have yet. Until then, a
downloaded copy needs the quarantine attribute cleared by hand. See
`docs/PACKAGING.md`.
