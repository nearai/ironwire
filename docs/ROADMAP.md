# Roadmap

Sequenced so that each milestone is independently shippable and each one proves
something the next depends on. The draft spec's V0 ("Claude Code + Codex → two
subscriptions + NEAR AI") is M1+M2+M3 here; every pairing in it except the two
diagonals needs the translation layer, which is where a schedule of that shape
dies (CRITIQUE §8).

---

## M1 — One native lane, end to end  ← current

**Claude Code → IronWire → Claude subscription, falling back to Anthropic API key.**

Zero translation. Proves the pipe.

Done and covered by tests:

- [x] Workspace, docs, trust posture
- [x] Credential discovery: Claude Code (Keychain + file); host-binding guard
- [x] `ironwire serve` — loopback daemon, token-gated control API
- [x] Anthropic façade: `/v1/messages` streaming + non-streaming, `/v1/messages/count_tokens`, `/v1/models`
- [x] Native passthrough with the exactly-enumerated mutation set (PROTOCOL §2)
- [x] Observation: usage from the SSE tee, quota from rate-limit headers
- [x] Before-first-byte failover, sticky affinity, descent hysteresis
- [x] Client-identity eligibility rule (TRUST §3)
- [x] Consent gate with recorded acceptance, versioned prompt (TRUST §2)
- [x] Passthrough conformance harness (PROTOCOL §7.2–7.3)
- [x] `ironwire connect claude` / `disconnect` / `status` / `doctor` / `env` / `pin`

Remaining before M1 closes:

- [ ] **Cancellation propagation on client disconnect.** The mechanism is in
      place (the tee flushes on `Drop`, and dropping a `reqwest` body stream
      aborts the request) but it is **not yet proved by a test** — PROTOCOL §4
      specifies the one to write. Until that test exists, treat this as
      unverified: an abandoned request that keeps generating burns the exact
      quota IronWire exists to protect.
- [ ] Token refresh on a 401 from the subscription backend (currently the
      credential is re-read from the store each request, which picks up Claude
      Code's own background refresh but does not drive one)
- [ ] Local trace ledger (SQLite) + `ironwire log`
- [ ] Live probe in `ironwire doctor` (PROTOCOL §7.4)
- [ ] Single-daemon lockfile and `ironwire serve` port-collision UX
- [ ] Agent-level acceptance test (PROTOCOL §7.5)

**Exit criterion:** an 8-hour Claude Code session runs through IronWire with no
observable behavioral difference, survives a rate-limit event by descending to
rung 1 then 2, and `ironwire status` shows quota numbers that match what the
Anthropic console reports.

---

## M2 — Second native lane

**Codex → IronWire → ChatGPT subscription / OpenAI API key.**

- OpenAI façade: `/v1/responses` (streaming), `/v1/chat/completions`
- Codex credential discovery via `ironclaw_llm::auth`
  (`CredentialSource::CodexCli`), client-version detection for model gating
- `ironwire connect codex` — writes the `[model_providers.ironwire]` block
- NEAR AI backend over Chat Completions
- Per-backend circuit breaker + cooldown
- Aggregate view: `ironwire status` shows all pools as one balance

**Exit criterion:** Codex and Claude Code both run through one daemon
simultaneously, each on its own native lane, with independent quota tracking.

---

## M3 — First translation pair

**Anthropic façade → OpenAI Chat Completions backend** (the well-trodden
direction), gated by `eligible()`.

- Canonical IR + capability matrix, wired to hard ineligibility rules
- Conversation-lifetime tool-ID map
- Rung-3 announcement UX — the user is told when family changes
- `ironwire pin` and `X-IronWire-Route`

**Exit criterion:** Claude Code, at rung 3 on a NEAR AI model, completes the
scripted acceptance task (PROTOCOL §7.5). Any conversation carrying signed
thinking blocks is correctly refused rung 3 rather than degraded.

---

## M4 — Distribution

- CI release matrix: macos-arm64/x64, linux-x64/arm64 (gnu + musl), windows-x64
- `cargo-dist`: shell installer, Homebrew tap, npm package, MSI
- `.deb` + hosted apt repo
- pip wheel with platform tags
- `ironwire update`

See [`PACKAGING.md`](./PACKAGING.md).

---

## M5 — macOS menu bar app

Thin SwiftUI client over `/_ironwire/status` and `/_ironwire/events`. Shows the
capacity bars, current route, live spend, and a pin control. **No routing logic
in Swift** — the daemon is the only brain (DESIGN §6).

---

## M6 — Trace contribution

- Wire `ironclaw_trace_commons` behind feature `contribute`
- Device-key enrollment via `ironwire connect near`
- Per-scope opt-in, manual-review holds surfaced in the CLI and the menu bar
- Credits reflected in `ironwire status` as a capacity pool

---

## Later

- Local backends (Ollama, vLLM, LM Studio) as a rung-3 target
- GitHub Copilot subscription backend (`ironclaw_llm::github_copilot_auth`)
- Bedrock / Vertex as rung-2 targets for the Anthropic family (same wire format,
  different credential — high-fidelity fallback)
- Hooks plugin adding `git diff` / test results / acceptance to traces
- Windows-native credential store support
