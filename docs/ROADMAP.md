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

### Stream resilience ✅ built

The three shapes of "API Error: Response stalled mid-stream" (`PROTOCOL.md` §5):

- [x] `ping` keepalives during upstream silence, with a give-up cap so we never
      ping at a hung upstream
- [x] Terminal `error` events instead of a dropped connection, so a truncated
      response is stated rather than inferred
- [x] Retry window widened to the first **content** byte — a failure during the
      thinking gap is restarted invisibly
- [x] Same-backend retry with backoff on 529/5xx/reset before descending the
      ladder, so a blip does not move a conversation onto a metered key
- [ ] Confirm the keepalive interval is comfortably under Claude Code's own
      stall timeout (15s is a conservative guess; the real value is not
      documented and should be measured)

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

## M3 — First translation pair  ✅ core done

**Anthropic façade → OpenAI Chat Completions backend** (NEAR AI and any
OpenAI-compatible endpoint), gated on the turn-boundary rule.

- [x] `ironwire_translate`: request, response, and streaming translation
- [x] Stateless reversible tool-id mapping (no per-conversation map to lose)
- [x] `mid_tool_loop` gate — switch families at a turn boundary only
- [x] `ChatCompletionsBackend` (NEAR AI + arbitrary OpenAI-compatible)
- [x] End-to-end test: `tests/claude_code_on_nearai.rs`
- [ ] Rung-3 announcement UX — the user is told when the family changes. Still
      open, and still the unresolved product question: IronWire has no UI channel
      into Claude Code (`docs/CRITIQUE.md` open questions).
- [ ] `ironwire pin` / `X-IronWire-Route` honoured for cross-family routes
- [ ] `ironwire connect near` (device-key enrolment; today the backend appears
      when `NEARAI_API_KEY` is set)

**Known limitation, deliberately not fixed yet:** a session cannot change
families *mid tool loop*. It waits for the next clean turn. Lifting this needs
the return path to synthesize the reasoning state a foreign assistant turn
lacks, and the exact tolerance for a missing block is undocumented — so it needs
validating against the live API before shipping. `docs/PROTOCOL.md` §6 has the
two candidate approaches.

**Exit criterion:** met at the wire level — Claude Code completes a turn on
NEAR AI, including a tool call whose id round-trips, and a mid-loop conversation
correctly waits rather than switching. Not yet run against live NEAR AI.

---

## M4 — Distribution and updates

- [ ] CI release matrix: macos-arm64/x64, linux-x64/arm64 (gnu + musl), windows-x64
- [ ] `cargo-dist`: shell installer, Homebrew tap, npm package, MSI
- [ ] `.deb` + hosted apt repo
- [ ] pip wheel with platform tags
- [ ] **Signed releases** — minisign/cosign, verified against a key in the
      binary. A checksum served from the same host as the binary proves nothing.

### Updates: notify-only ✅ built

`ironwire` never updates its own binary — it is a daemon holding credentials in
the middle of a streamed response (`docs/UPDATES.md` §1).

- [x] `InstallMethod::detect` — defer to whoever owns the install (`brew
      upgrade`, `apt install --only-upgrade`, …). Self-updating a managed
      install desyncs its package manager.
- [x] Once-a-day check, cached, never blocking startup, kill switch honoured
      before the first request
- [x] `minimum_supported` floor, so "probably broken" reads differently from
      "a bit old"
- [x] `ironwire update`; `ironwire status` surfaces it
- [ ] Publish `manifest.json` at the pinned URL (needs the release job)

### Provider-quirks channel ✅ built

Signed data, refreshed independently of the binary, so a changed
`anthropic-beta` flag is a minutes-long fix rather than a five-ecosystem release
(`docs/UPDATES.md` §2).

- [x] Bounded schema — **no field can express a host, URL, or path**, so
      `TRUST.md` I2 holds even against a compromised signing key
- [x] ed25519 verification before parse, rollback guard on `serial`, fail-closed
      onto compiled-in defaults
- [x] Wired into the Anthropic protocol constants and the client-identity markers
- [ ] Real signing key + a published document (the compiled-in key is a
      placeholder that verifies nothing, so the channel is inert until then)
- [ ] Periodic refresh in the daemon (today it loads at startup)

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
