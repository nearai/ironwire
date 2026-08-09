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

- ✅ OpenAI façade: `/openai/v1/responses`, `/openai/v1/chat/completions`,
  `/openai/v1/models` (`ironwire_proxy::facade::openai`)
- ✅ `ResponsesBackend` — ChatGPT subscription and metered OpenAI key on one
  wire, so falling between them costs money and nothing else
- ✅ Codex credential discovery **delegated** to `ironclaw_llm::auth`
  (`CredentialSource::CodexCli`), plus the `chatgpt-account-id` header derived
  from the credential itself — Codex omits it on a custom-provider path and the
  subscription rejects requests without it
- ✅ `ironwire connect codex` / `ironwire disconnect codex` — edits
  `~/.codex/config.toml` as *text*, so comments and hand-edits survive, backs
  up the previous contents, and restores the previous `model_provider` on
  disconnect (`src/codex_config.rs`)
- ✅ NEAR AI backend over Chat Completions (landed with M3)
- ✅ Per-backend circuit breaker (`ironwire_upstream::breaker`), wired into
  routing so a dead backend is not rediscovered every turn — with the
  deliberate exception that the last backend standing is still tried
- ✅ Aggregate view: `ironwire status` shows all pools as one balance —
  counted, not summed, because the windows share no unit
- ✅ Codex `client_version` detection (`ironwire_upstream::codex_version`) —
  ported, since ironclaw's lives in a private module. `codex --version` under a
  2s bound, cached per process, with `IRONWIRE_CODEX_CLIENT_VERSION` for a
  daemon whose `PATH` is not the user's shell `PATH`. Absent Codex is a normal
  state, not an error
- ✅ The catalogue is now *asked for* rather than compiled in: `probe` reads
  `/models?client_version=` and remembers the answer, so `Backend::models()`
  reflects what this account is actually entitled to. An unreadable or empty
  response leaves the compiled-in list in force — the position we were in
  before asking
- ✅ Expired-token handling — and a **deliberate decision not to refresh**.
  `ironclaw_llm::codex_auth::refresh_access_token` writes back to
  `~/.codex/auth.json`, and that file is Codex's, not ours — and now *both*
  Codex clients': the CLI and the desktop app share it, so IronWire would be a
  third participant. A writer racing Codex's own refresh can log a user out of
  the tool they actually paid
  for, and the failure would look like Codex's bug rather than IronWire's. So
  IronWire re-reads the file every request, picks up whatever Codex wrote, and
  when the token really is stale says so in one sentence with one command to
  run — rather than presenting it and handing the user a 401 to interpret
- ⬜ Verify against a real ChatGPT subscription. Every assertion in
  `tests/codex_on_subscription.rs` runs against a mock; the header set the live
  backend actually requires is unverified

**Exit criterion:** Codex and Claude Code both run through one daemon
simultaneously, each on its own native lane, with independent quota tracking.
*Not yet met* — the lane is built and tested against a mock, but has not been
run against a live ChatGPT subscription.

---

## M3 — First translation pair  ✅ core done

**Anthropic façade → OpenAI Chat Completions backend** (NEAR AI and any
OpenAI-compatible endpoint), gated on the turn-boundary rule.

- [x] `ironwire_translate`: request, response, and streaming translation
- [x] Stateless reversible tool-id mapping (no per-conversation map to lose)
- [x] `mid_tool_loop` gate — switch families at a turn boundary only
- [x] `ChatCompletionsBackend` (NEAR AI + arbitrary OpenAI-compatible)
- [x] End-to-end test: `tests/claude_code_on_nearai.rs`
- [x] Rung-3 announcement UX, via a **side channel** rather than an in-band one.
      IronWire's only writable channel into a coding agent is the response
      stream, and putting a line there would put words in the model's mouth and
      corrupt the transcript the agent stores and replays. So: `/_ironwire/events`
      (SSE) and `ironwire watch [--only-changes]`, which prints nothing on a
      healthy system and one line when the family changes. The bus is lossy and
      non-blocking by construction — the same rule as the observation tee
- [x] `ironwire pin` and `X-IronWire-Route` honoured for cross-family routes.
      The header is per-request and outranks the daemon-wide pin; it is stripped
      before forwarding, and naming a backend that does not exist is a 400 that
      lists what is available rather than a silent fall-through
- [x] `ironwire connect near`, plus `connect anthropic-api` / `connect openai-api`.
      Device-key enrolment still belongs to M6; today the key is all that is
      needed, and the command says so rather than implying more

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

- [x] CI release matrix: macos-arm64/x64, linux-x64/arm64 (gnu + musl),
      windows-x64 — `.github/workflows/release.yml`, tag-driven, with a
      version/tag consistency check that fails *before* the matrix runs
- [x] Shell installer (`scripts/install.sh`) — POSIX sh, checked under dash,
      because it is the fallback for platforms the package managers miss and
      that includes machines without bash
- [x] Homebrew formula generator (`packaging/build_brew.py`) with a
      `brew services` definition and generated completions
- [x] npm: shim + os/cpu-gated per-platform packages, **no postinstall script**
- [x] pip: one wheel per platform tag, no sdist (an sdist would promise a
      from-source build that does not exist)
- [x] `.deb` via nfpm, carrying a systemd **user** unit
- [x] `manifest.json` generator; structurally cannot express a URL or host
- [x] `ironwire service install|uninstall|status` — launchd / systemd-user /
      schtasks. Always a user agent, and there is deliberately no flag to ask
      for a system service
- [x] `ironwire completions <shell>`
- [x] Every packaging script is exercised on **every CI push** against fake
      release artifacts (`scripts/test-install.sh`, `scripts/test-packaging.sh`)
      — they otherwise only run inside a tag build, which is the worst place to
      find a typo
- [ ] **Signed releases** — minisign/cosign, verified against a key in the
      binary. A checksum served from the same host as the binary proves nothing.
      *Needs a real signing key; see "Blocked on infrastructure" below.*
- [ ] Hosted apt repo at `apt.ironwire.dev`. *Needs hosting.*
- [ ] Windows MSI. The `.zip` and `winget` cover the same ground; revisit if
      anyone asks.

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
- [x] Periodic refresh in the daemon: a document that needed a restart to take
      effect would have the same latency as a release, which is the problem the
      channel exists to solve. Six-hourly, first check delayed a minute so it
      never competes with the first request, and governed by the same
      `updates.check` switch — someone who turned one network call off meant
      both. A failed fetch leaves the previous document in force

---

## First-run and daily use

The failures that actually cost people their first twenty minutes, none of
which are routing problems.

- [x] `ironwire doctor` checks the **clients**, before the backends. Every
      backend can be green while the agent goes straight to the provider
      because nothing points it here — the commonest real failure, and the one
      the old output could not distinguish from working. It names the near-miss
      cases too: a stale port in `ANTHROPIC_BASE_URL` or in Codex's config is a
      silent bypass whose symptom is identical to the daemon being down
- [x] `ironwire env` emits the syntax of the shell you are actually running,
      defaulting from `$SHELL`. Piping `export FOO=bar` into fish's `eval`
      gives a syntax error and no clue why
- [x] `ironwire service install|uninstall|status`, `ironwire completions`
- [x] `ironwire watch` for live routing, `ironwire privacy check` for the filter
- [x] `ironwire init` — says what capacity this machine already has and what to
      run, in order. It is not a wizard: it changes nothing without `--write`,
      and it never grants a subscription consent, because a command that did
      would make the consent prompt decorative
- [x] `ironwire init --write` drops a commented `config.toml`. A test asserts
      the generated file parses **and equals the built-in defaults** — a
      template that drifts turns "a file documenting what IronWire does" into
      "a file that quietly changes what IronWire does"

---

## M5 — macOS menu bar app

Thin SwiftUI client over `/_ironwire/status` and `/_ironwire/events`. Shows the
capacity bars, current route, live spend, and a pin control. **No routing logic
in Swift** — the daemon is the only brain (DESIGN §6).

Built, in [`macos/`](../macos/README.md). The icon carries four states, the
dropdown mirrors `ironwire status`, and an unobserved backend renders the word
`unknown` with no bar — a bar drawn at zero would be a number nobody measured, in
the one surface a user takes in without reading.

It ships as `IronWire-macos.zip` and installs through the Homebrew formula. What
is outstanding is **signing and notarisation**: the bundle is ad-hoc signed,
which is enough for `brew` and for a local build but not for a direct download.
That needs the Apple Developer certificate in the table below.

---

## M6 — Trace contribution

- Wire `ironclaw_trace_commons` behind feature `contribute`
- Device-key enrollment via `ironwire connect near`
- Per-scope opt-in, manual-review holds surfaced in the CLI and the menu bar
- Credits reflected in `ironwire status` as a capacity pool

---

## Compaction-aware routing

Found while designing M7, but not a privacy issue — worth doing on its own.
`docs/PROTOCOL.md` §8 has the reasoning.

A compaction turn's output *becomes the conversation*: it is written into the
client's permanent history and resent every turn afterwards. Degrading it to
save money buys one cheaper request and pays for it for the rest of the session.

- [x] Fidelity dominates marginal cost on a compaction turn: the candidate sort
      swaps its two middle keys, so a metered backend that serves the requested
      tier outranks free capacity that would descend
- [x] A compaction turn does not inherit a *degraded* affinity. An undegraded
      one is kept — moving would throw away a warm cache for nothing
- [x] Availability still outranks fidelity, and cross-family is still served if
      it is all that is left: refusing would leave the user unable to compact
      at all, which ends the session
- [x] `compaction_stall_timeout_secs` (600s), floored at the ordinary timeout so
      a misconfiguration cannot make compaction *more* fragile than a normal turn
- [x] Recognition lives in the quirks channel and is advisory: a miss costs a
      slightly worse routing decision on one turn, never a wrong answer. An
      empty marker list disables it
- [ ] **The marker set is a conservative guess, not a verified fingerprint.** No
      harness documents its compaction prompt; the real strings need observing
      per harness and shipping through the quirks channel

---

## M7 — Privacy filter

**Optional, off by default.** Remove sensitive values on the way out, restore
them on the way back, so an agent can run against a provider the user does not
fully trust. Design and critique in **`docs/PRIVACY.md`**; that document is the
spec and this is the checklist.

The order below is deliberate: the reversal machinery and its test suite come
*before* any detector beyond the trivial one, because reversal is where every
hard failure lives and a detector is worthless without it.

### Foundation — reversal

- [x] `ironwire_privacy` crate: deterministic
      `HMAC(conversation_salt, plaintext) → placeholder`, map derived per
      request and never persisted (PRIVACY §4). No `save`, no `load`, no
      `Serialize`
- [x] Substitution over parsed request bodies, both façades, preserving field
      order and leaving non-string leaves alone
- [x] Streaming reversal across SSE chunk boundaries, bounded buffer, tested at
      **every** byte offset plus one-chunk-per-character, with UTF-8
      reassembly for codepoints split across chunks
- [x] **Fail loudly on partial reversal.** Caught a real bug: the reconnect
      path inside `resilience::guard` bypassed the reverser, so a restarted
      stream forwarded raw placeholders. Both façades now reverse the restart
      too
- [x] Per-conversation salts, bounded and memory-only, stable across turns so
      the provider's prompt cache is not destroyed every request
- [x] Filter state on its own permanent line in `ironwire status`
- [x] Per-exchange substitution counts in the ledger and `ironwire log`. `None`
      (filter off) is never conflated with `0` (on, found nothing) — the second
      is the signal a user needs to see

### Tier 1 — secrets (deterministic)

- [x] Reuse `ironclaw_safety::LeakDetector` and its pattern set directly
- [x] Map its one-way `LeakAction::Redact` onto our reversible substitution

### Tier 2 — named values (deterministic)

- [x] User-nominated exact strings via `redaction_values_for_secret`, which
      already expands URL-encoded variants
- [x] `ironwire privacy check <file>` / `privacy status` — show what would and
      would not be caught, so the false-negative rate is something a user can
      see. Prints "no matches", never "clean", and previews rather than
      reprinting matched values into a terminal that may be screen-shared

### Compaction — one case per harness

Every harness compacts, and a compaction summary becomes *permanent* client-side
history. An unreversed placeholder there is self-perpetuating corruption
(PRIVACY §5). Correctness must not depend on recognizing a compaction request.

- [x] Claude Code (`/v1/messages`), Codex (`/v1/responses`), Aider
      (`/v1/chat/completions`) — `tests/privacy_compaction.rs`. Cline/Roo speak
      whichever façade they are pointed at, so the two wires above cover them;
      a third fixture would test the same code twice
- [x] Mangled-placeholder case for each: the damaged summary never reaches the
      client, so it can never become permanent history
- [x] Stale placeholder from a previous salt is passed through, never
      mis-reversed — asserted across a simulated daemon restart
- [x] Compaction does not change the conversation key, so the salt and the
      route survive it. If it did, every compaction would silently re-roll both
      at the worst possible moment
- [ ] Optional compaction fingerprints in the **quirks channel**, as an
      optimization only — a client-shape fingerprint is exactly the thing that
      breaks silently on a client update
- [x] Detection cost **measured rather than assumed**: tiers 1 and 2 cost ~1.5 ms
      per turn on a 333 KB history, so the per-block cache this document
      originally planned is not built — and the correctness hazard it would have
      introduced across a compaction boundary does not exist. The budget is an
      assertion in `ironwire_privacy/tests/cost.rs`; tier 3 will need the cache

### Tier 3 — inferred PII (local model, non-deterministic)

- [ ] Local classifier, offline; a test asserts the path makes **no** network
      call
- [ ] Bounded per-request latency; exceeding the budget degrades to tier 2 and
      says so rather than stalling the agent
- [ ] Published precision/recall against a labelled corpus. A tier that cannot
      state its error rate does not ship

### False positives — the coding-agent-specific hazard

Much of what a PII detector flags in a coding session is load-bearing *code*.

- [x] Never substitute inside fenced code blocks or `tool_result` content by
      default; both are switchable
- [x] Never substitute reserved ranges (`example.com`, RFC 5737, RFC 1918,
      RFC 3849, `555-01`) — including the `172.16/12` boundary, which a prefix
      test alone gets wrong
- [x] Corpus test: real-looking fixtures pass tiers 1–2 with zero
      substitutions, paired with a fixture set of genuine secrets so the corpus
      test cannot be satisfied by matching nothing. It found a real false
      positive — `high_entropy_hex` matching lockfile checksums and git SHAs,
      which now require a nearby credential word
- [ ] Decide opaque tokens vs. format-preserving surrogates **with data** —
      opaque tokens preserve the value but destroy the structure the model needs
      to reason about (PRIVACY §6)

**Exit criterion:** a full Claude Code session, including at least one
compaction, runs end to end with tiers 1+2 on; the provider receives no
nominated value; the client's transcript contains no placeholder; and
`tests/passthrough.rs` still passes with the filter off.

### ✅ Local backends (Ollama, vLLM, LM Studio)

`kind = "local"` in `config.toml`, over the OpenAI-compatible surface only —
Ollama's native `/api/*` is a different protocol. No auth by default, because
most local servers take none; `BackendKind::Local`, so it is free at the margin,
not consent-gated, and never priced (a local slug hitting the cloud price table
would report money nobody spent).

The rule that makes it safe: **a local model counts as `fast` unless the user
says otherwise**, and a local backend never outranks one that can actually serve
the tier asked for. Local capacity sorts cheapest of all, and
`ModelTier::from_model_hint` resolves an unrecognised slug to `frontier` — so
without both of those, `qwen3-coder:30b` would quietly take every request meant
for Opus. `ironwire init` reports a server on the standard ports and writes
nothing.

---

## Later

- GitHub Copilot subscription backend (`ironclaw_llm::github_copilot_auth`)
- Bedrock / Vertex as rung-2 targets for the Anthropic family (same wire format,
  different credential — high-fidelity fallback)
- Hooks plugin adding `git diff` / test results / acceptance to traces
- Windows-native credential store support

---

## Blocked on infrastructure or a live account

Not "not done yet" — these cannot be finished from a development machine, and
listing them here keeps them from looking finished when they are not.

| Item | Needs | Milestone |
|---|---|---|
| ~~Verify the ChatGPT lane end to end~~ ✅ done — `scripts/acceptance.sh` completes a real task on a live ChatGPT Pro account. It found that Codex 0.145 sends no `instructions` field, that the endpoint URL was built wrongly for the real host, and that `/models` is a different document from the public API's | M2 |
| ~~Verify the Claude lane end to end~~ ✅ done — same script, a live Claude Max account. It found the identity marker rotted, the model substituted from a stale catalogue, and the rate-limit headers read under names Anthropic does not send | M1 |
| The Codex desktop app | the app itself, to capture its `originator` and `instructions`; the CLI is verified and the app shares its config and credentials but not its HTTP stack | M2 |
| Measure Claude Code's real stall timeout | a live session; `keepalive_secs` is currently a guess | M1 |
| An 8-hour session including a real rate-limit descent | time, and a subscription to exhaust. Still the largest untested path: no provider has rate-limited us, so descent under real exhaustion, the cross-family lane and the climb back up are mock-only | M1 |
| Signed releases | a real signing key, held somewhere that is not a repo | M4 |
| Publish `manifest.json` | a pinned URL to serve it from | M4 |
| Hosted apt repo | hosting, plus a signing key for the `Release` file | M4 |
| Real quirks signing key | same. The compiled-in key is a placeholder that verifies nothing, so the channel is inert — which is the correct failure mode, but it *is* inert | M4 |
| npm / PyPI / tap publication | `NPM_TOKEN`, PyPI trusted publishing, `TAP_TOKEN` | M4 |
| Menu bar app signing | an Apple Developer certificate. The app is built and ad-hoc signed, which is enough for `brew` and for a local build; a *downloaded* bundle is blocked by Gatekeeper until it is notarised | M5 |
| Tier-3 PII classifier | a local model, and a labelled corpus to state its precision and recall against | M7 |

The code paths for these exist and are tested against mocks and fixtures. What
is missing is the last mile that only a real credential, a real key, or real
hosting can provide.
