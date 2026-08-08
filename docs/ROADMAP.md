# Roadmap

Sequenced so that each milestone is independently shippable and each one proves
something the next depends on. The draft spec's V0 ("Claude Code + Codex → two
subscriptions + NEAR AI") is M1+M2+M3 here; every pairing in it except the two
diagonals needs the translation layer, which is where a schedule of that shape
dies (CRITIQUE §8).

---

## M1 — One native lane, end to end  ✅ complete

**Claude Code → IronWire → Claude subscription, falling back to Anthropic API key.**

Zero translation. Proves the pipe.

- [x] Workspace, docs, trust posture
- [x] Credential discovery: Claude Code (Keychain + file); host-binding guard
- [x] `ironwire serve` — loopback daemon, token-gated control API
- [x] Anthropic façade: `/v1/messages` streaming + non-streaming, `/v1/messages/count_tokens`, `/v1/models`
- [x] Native passthrough with the exactly-enumerated mutation set (PROTOCOL §2)
- [x] Observation: usage from the SSE tee, quota from rate-limit headers
- [x] Before-first-byte failover, sticky affinity, descent hysteresis
- [x] Client-identity eligibility rule (TRUST §3)
- [x] Consent gate with recorded acceptance, versioned prompt (TRUST §2)
- [x] Cancellation propagation, **proved** by `tests/cancellation.rs`
- [x] Credential re-read and single retry on a subscription 401
- [x] Local trace ledger (SQLite) + `ironwire log`
- [x] Live probe in `ironwire doctor` (PROTOCOL §7.4)
- [x] Port-collision UX that distinguishes "IronWire already running" from
      "something else holds this port"
- [x] Conformance: passthrough (§7.2–7.3) and a three-turn tool loop (§7.5,
      automatable half)
- [x] `ironwire connect` / `disconnect` / `status` / `doctor` / `log` / `env` / `pin`

Carried into M2 (needs a live account, not a mock):

- [ ] `scripts/acceptance.sh` — run a real Claude Code task through IronWire and
      compare its turn count against a direct run. Written; **not yet executed
      against a live account.** Until someone runs it, "no observable
      behavioural difference" is an inference from wire-level tests, not an
      observation.
- [ ] An 8-hour session survived, including a real rate-limit descent

**Exit criterion (wire level, met):** every mutation is enumerated and pinned by
a byte-identity test; a three-turn tool loop with signed thinking blocks and
replayed tool ids survives intact; an abandoned request provably stops
generating upstream; observed quota comes from provider headers or reads
`unknown`.

**Exit criterion (field level, open):** an 8-hour Claude Code session with no
observable behavioural difference, surviving a real rate-limit descent, with
`ironwire status` matching the Anthropic console. That needs a live account —
see the carried items above.

---

## M2 — Second native lane  ← current

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
