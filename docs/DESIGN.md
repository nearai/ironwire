# IronWire — design

> IronWire turns your AI subscriptions, API keys and local compute into one
> local inference endpoint. Point Claude Code, Codex or any AI tool at
> localhost; IronWire keeps it on the best available model and degrades
> gracefully when limits hit — so your agent never dies at the rate limit.

This is the finalized design. The review that produced it is in
[`CRITIQUE.md`](./CRITIQUE.md); the trust/consent posture it depends on is in
[`TRUST.md`](./TRUST.md); wire-level detail is in [`PROTOCOL.md`](./PROTOCOL.md).

---

## 1. What IronWire is, and is not

**Is:** a loopback HTTP daemon that presents native provider APIs to coding
agents, and forwards each request to whichever of the user's own capacity pools
can serve it with the required fidelity.

**Is not:** an agent, a harness, a conversation store, a hosted service, or a
credential custodian. The coding agent keeps the loop, the context, compaction,
tool execution and file state. IronWire sees a sequence of inference requests
and decides where each conversation goes.

The load-bearing consequence: **IronWire never needs to understand the task.**
It needs to understand the *wire*.

```
┌─────────────────────────────────────────────────────────┐
│  Claude Code   Codex   Cline   Aider   Zed   custom      │
└────────┬──────────────────┬─────────────────────────────┘
    Anthropic Messages   OpenAI Responses / Chat Completions
         │                  │
         ▼                  ▼
   ┌───────────────────────────────────────┐
   │  IRONWIRE  (127.0.0.1:8463)           │
   │                                       │
   │  façades ── policy ── backends        │
   │      │        │          │            │
   │  observe   fidelity   credential      │
   │  usage &   ladder     resolution      │
   │  limits    + quota    + refresh       │
   │                                       │
   │  local trace ledger (SQLite)          │
   └──────────────┬────────────────────────┘
                  │
   ┌──────┬───────┼────────┬──────────┬─────────┐
   ▼      ▼       ▼        ▼          ▼         ▼
Claude  ChatGPT  Anthropic OpenAI   NEAR AI   local
sub     sub      API key   API key            (ollama/
(OAuth) (OAuth)                                vLLM)
```

---

## 2. Two lanes

This is the central architectural decision. See CRITIQUE §3.

### Native lane — the default path

Inbound protocol == backend protocol. IronWire does **not** deserialize and
re-serialize the body. It:

1. Peeks at the fields it needs for policy (`model`, `stream`, message count,
   approximate size, presence of `cache_control` / `thinking` / images).
2. Chooses a backend.
3. Rewrites the auth headers and the URL; adjusts `model` only if the policy
   changed it.
4. Streams the request body up and the response body down **verbatim**,
   frame-for-frame for SSE.
5. Observes usage and rate-limit state from response headers and from a
   read-only tee of the SSE stream.

Fidelity is 1.0 by construction, including for provider features released after
we shipped: unknown fields survive because we never look at them.

Native pairs in the plan:

| Façade | Backend | Status |
|---|---|---|
| `/anthropic/v1/messages` | Claude subscription (OAuth) | M1 |
| `/anthropic/v1/messages` | Anthropic API key | M1 |
| `/openai/v1/responses` | ChatGPT/Codex subscription (OAuth) | M2 |
| `/openai/v1/responses` | OpenAI API key | M2 |
| `/openai/v1/chat/completions` | NEAR AI, OpenAI-compatible, Ollama | M2 |

### Translated pairs

Every ordered pair of the three wires, through a canonical IR rather than a
translator per pair (`docs/TRANSLATION.md`). All of them are gated on the same
turn-boundary rule.

| From | To | Cost beyond a native forward |
|---|---|---|
| `anthropic.messages` | `openai.responses` | signed thinking, cache breakpoints |
| `anthropic.messages` | `openai.chat` | signed thinking, cache breakpoints, system flattened |
| `openai.responses` | `anthropic.messages` | encrypted reasoning |
| `openai.responses` | `openai.chat` | encrypted reasoning, typed items flattened |
| `openai.chat` | `anthropic.messages` | — |
| `openai.chat` | `openai.responses` | — |

### Translated lane — fallback only

Inbound protocol != backend protocol. IR-mediated, and **capability-gated: a
route that cannot preserve the request's semantics is ineligible, not
best-effort.**

```rust
/// A route is eligible only if the target can serve the request without
/// *breaking* it. A route that would merely be worse is not refused — it is
/// announced.
struct RequestRequirements {
    tools: bool,
    parallel_tool_calls: bool,    // history already issues several calls per turn
    images: bool,
    reasoning: ReasoningNeed,     // informational: continuity lost, not illegal
    prompt_cache: bool,
    cached_prefix_tokens: u32,
    structured_output: bool,
    min_context_tokens: u32,
    mid_tool_loop: bool,          // the cross-family gate
}
```

The one cross-family correctness rule is **switch at a turn boundary, never mid
tool loop**. A conversation caught mid-loop waits for the next clean turn; it is
not disqualified. Provider-private reasoning state (signed Anthropic thinking,
encrypted OpenAI reasoning) is a *quality* signal, not an eligibility one — a
foreign provider never validates it and the originating API drops rather than
rejects it. `docs/PROTOCOL.md` §6 has the full reasoning, including why an
earlier version of this document got it wrong.

---

## 3. Routing: per-conversation, not per-request

### Conversation identity

IronWire has no session ID from the client. It derives one:

```
conversation_key = H(façade, first_system_block_prefix, first_user_message, tool_name_set)
```

with a rolling secondary match on the longest common message prefix, so a
growing conversation keeps its key across turns. Keys are memory-only,
TTL-evicted, and never persisted with content.

Each conversation gets a sticky `RouteAffinity { backend, model, since, rung }`.

### The fidelity ladder

Fallback is a state transition on the conversation, taken under pressure, and
never reversed within a conversation unless the higher rung recovers *and* the
conversation is at a safe boundary (no in-flight signed reasoning state).

| Rung | Transition | Cache | Reasoning state | User told? |
|---|---|---|---|---|
| 0 | stay | warm | intact | no |
| 1 | same account, smaller model | mostly warm | intact | no |
| 2 | same wire format, different credential | cold | intact | no |
| 3 | different family (needs translation) | cold | dropped | **yes** |

Hysteresis: a rung change requires the trigger to persist past a debounce
window, so a single 429 with a 3-second `retry-after` waits rather than
resetting a 200k-token cache.

### Inputs to the decision

| Input | Source | Trust |
|---|---|---|
| requested model | request body | client hint → tier |
| requirements | body peek | authoritative |
| approximate prompt size | body byte length + message count | heuristic |
| observed quota | provider rate-limit headers | authoritative, with age |
| recent errors | circuit breaker per backend | authoritative |
| latency (TTFT p50) | local measurement | authoritative |
| user policy | config + `ironwire pin` | overrides all |
| complexity score | `ironclaw_llm::smart_routing` (optional) | weak signal, tier only |

The client's model string is a **quality hint mapped to a tier**
(frontier / balanced / fast), not a hard selection — Claude Code cannot type
`ironwire/auto`. Escape hatches: `ironwire pin`, the `X-IronWire-Route` header,
and `ironwire/*` slugs for clients that accept free-form model strings.

### Default policy

1. Serve at the requested tier from the highest-fidelity backend that is
   authenticated, in-quota and healthy.
2. Prefer marginal-cost-zero capacity (subscriptions) over metered.
3. Under quota pressure, descend the ladder — do not jump.
4. Never interrupt the client: a request that cannot be served at *any* rung
   returns a protocol-correct error the client already knows how to handle,
   with `retry-after` preserved.

---

## 4. Observed quota, never estimated

`QuotaSnapshot` is only ever built from provider-supplied values:

```rust
enum Headroom {
    /// Provider told us. Carries when.
    Observed { used_pct: f32, resets_at: Option<DateTime<Utc>>, observed_at: DateTime<Utc> },
    /// We are inside a 429's retry-after window.
    Exhausted { until: DateTime<Utc> },
    /// Backend authenticated but has told us nothing yet.
    Unknown,
}
```

`Unknown` is displayed as `unknown`. There is no fourth variant for a guess.
Metered backends (API keys) report *spend*, which we compute from observed
usage tokens × the price table (`ironclaw_common::llm_costs`), and label as an
estimate because it is one.

### 4a. What we *do* measure: our own traffic

There is a second question the quota rule does not answer, and users ask it
constantly: **how fast is this going, and will it last?** A rate-limit header
says where you are; it does not say whether the next hour is affordable.

`ironwire_usage` answers that from the trace ledger — the tokens IronWire
watched go past, cut into session windows, differentiated into a burn rate and
carried forward to the window's close. This is not an exception to §4, for one
structural reason: **it never claims to describe a provider's books.** It
describes the requests this machine made. `Headroom` gains no variant, nothing
computed there reaches routing or eligibility, and `capacity:` on the status
screen still says `unknown` for any pool the provider has not spoken about.

Every figure it produces carries a `Basis`:

| Basis            | Means                                     | Example |
| ---------------- | ----------------------------------------- | ------- |
| `Measured`       | summed from the ledger; happened          | `100.0k tokens · 12 exchanges` |
| `Projected`      | a measured rate × the time left           | `2.5M tokens by the time it closes` |
| `SelfCalibrated` | the user's own completed windows          | `your own p90 over 14 past sessions` |
| `Declared`       | a limit the user wrote in their config    | `the Max 5× limit you declared` |

There is deliberately no fifth variant meaning "a limit we assumed". Published
per-window token limits do not exist; the figures in circulation are
reverse-engineered. So IronWire ships that table (`usage::plan`) but never
consults it unless `usage.plan` is set — at which point the ceiling is the
user's claim about their own subscription, and the screen says so. Unset, the
comparison is against their own history, which needs no table. A backend with
neither yields no percentage and no bar rather than a plausible one.

The algorithms — five-hour windows rounded to the hour, gap blocks,
overlap-weighted hourly rate, and a p90 taken over sessions that look like they
hit a cap — are ported from
[`Maciek-roboblog/Claude-Code-Usage-Monitor`](https://github.com/Maciek-roboblog/Claude-Code-Usage-Monitor)
(MIT), which worked them out against real Claude Code transcripts. The
percentile deliberately matches Python's `statistics.quantiles` estimator so
the two tools agree on the same machine. One behaviour is not carried over:
there, a lone request reports its whole token count as a per-minute rate. One
request is a point, not an interval, so here it reports no rate at all.

---

## 5. Eligibility and identity

A subscription backend is eligible for an inbound request **only when the
request already carries that product's client identity.** Concretely: the
Claude-subscription backend serves requests whose first system block is Claude
Code's; the ChatGPT-subscription backend serves requests that arrive as Codex.

IronWire does not synthesize that identity for other clients. Aider pointed at
the Anthropic façade routes to an API key, NEAR AI, or a local model — not to
the Claude subscription. This is the difference between *routing the traffic a
subscription was sold for* and *dressing one client up as another*, and it is a
hard rule, not a default. See [`TRUST.md`](./TRUST.md).

---

## 6. Component map

```
crates/
  ironwire_core       types, config, capability gate, policy, quota
  ironwire_creds      credential discovery + consent
  ironwire_ledger     the local trace ledger
  ironwire_usage      session windows, burn rate, projections (§4a)
  ironwire_translate  cross-family translation (the fallback lane)
  ironwire_upstream   Backend trait; native passthrough clients; observation
  ironwire_proxy      axum façades, router wiring, control API, trace sink
src/
  the `ironwire` binary — the workspace root package, so `cargo run` and
  `cargo install --path .` work without knowing the layout
```

Dependency direction is strictly downward; `core` depends on nothing of ours.

### Where backends come from

Discovery is the product: a fresh install with a Claude Code login has working
backends and an empty `config.toml`, and `init --write` generates no
`[[backends]]` section on purpose. Configuration adjusts that; it does not
replace it.

```
for each [[backends]] entry:
    enabled = false          → never registered, and not shown in status
    id matches a discovered backend
                             → discovery builds it; the entry overrides
                               base_url, api_key_env and models
    any other id             → built from `kind`
then: every backend discovery can produce that no entry named
```

Config-declared backends are appended after the discovered ones. Registration
order is the tie-break in `Policy::select`, so putting them first would silently
change which backend wins a tie for everyone who already had a config.

A backend whose credential is missing is still registered, so `status` can say
"not logged in" rather than omitting it — which reads as though IronWire never
heard of it. `enabled = false` is a different statement, and does omit it.

`kind` must be one of `claude-subscription`, `anthropic-api`,
`codex-subscription`, `openai-api`, `nearai`, `openai-compatible`. An unknown
kind, a duplicate id, or an `openai-compatible` entry without both `base_url`
and `api_key_env` fails at load, naming the entry — before the port is bound,
because a backend that silently never appears is the worst way to learn about a
typo.

### The daemon and its control API

One daemon per machine, lockfile at `$IRONWIRE_HOME/daemon.lock`, listening on
`127.0.0.1:8463`. Everything except the two façade prefixes lives under
`/_ironwire/`:

```
GET  /_ironwire/status          full state snapshot (JSON)
GET  /_ironwire/backends        the same handler as /status
GET  /_ironwire/settings        what can be changed, what is selectable, and
                                which coding agents are wired to us
GET  /_ironwire/log?limit=      recent exchanges from the local ledger,
                                also ?since= and ?after_id= to page
GET  /_ironwire/events          SSE: route decisions, quota changes, errors
GET  /_ironwire/health          liveness. The one route with no token
POST /_ironwire/pin             { backend, model } — no backend clears the pin
POST /_ironwire/privacy         { mode }
POST /_ironwire/consent         { backend, granted, prompt_version }
POST /_ironwire/tools           { id, connect } — point a coding agent here
POST /_ironwire/probe           hit every backend for real
```

There is no shutdown route. The daemon stops on a signal, which is what a
service manager sends anyway; an HTTP endpoint that ends the process would be
one more thing the control token has to be trusted with.

`ironwire status` and the macOS menu bar app are both **clients of this API**.
No routing logic lives in the CLI or in Swift; otherwise they diverge.

The tool list on `/settings` is that rule applied to detection. `ironwire
connect` is what edits somebody's agent config, so IronWire is the only thing
that knows which agents are here and which are pointed at it; both the CLI and
the API read it from one place (`ironwire_agents::tools`). A client that
detected agents for itself would be a second answer to that question on the
same machine, and the wrong one.

That rule is why `/settings` reports which privacy modes are *selectable* rather
than only which exist. `full` routes solely to backends the user has named, so
with none named it would take every backend out of service — a rule that lives
in `Config::validate`. A client that worked it out for itself would be a second
implementation of it, in a language that cannot see the config.

The two writes follow from the same principle. `POST /privacy` changes the
running daemon *and* `config.toml`, because a change that only applied at the
next restart is a switch that appears to do nothing, and one that only lived in
memory would quietly revert the user onto a weaker filter. `POST /consent`
carries the prompt version the user was actually shown and refuses anything
else: an answer belongs to the wording it answered (`docs/TRUST.md` §2).

---

## 7. Reuse of `nearai/ironclaw`

Reuse is deliberate and layered, because binary size is a distribution
constraint (`brew`/`npx`/`apt`/`pip` all want one small self-contained
executable) and `ironclaw_llm` pulls `rig-core`.

| What | From | Status |
|---|---|---|
| Codex credential discovery, `auth.json` format, ChatGPT-vs-key base URL split | `ironclaw_llm::auth::load_persisted_credentials` (`CredentialSource::CodexCli`) | **delegated** — `ironwire_creds::codex` is a thin wrapper. Two exceptions, both noted in that module: the `CODEX_HOME` override (ironclaw hardcodes `~/.codex`) and the `chatgpt_account_id` claim (ironclaw's extractor is private) |
| Price table | `ironclaw_common::llm_costs::price_usage` | **delegated** — `ironwire_ledger::price`. Closed a real gap: `spend_today_usd` was structurally always `None` before |
| Circuit-breaker vocabulary (`CircuitState`, `CircuitBreakerConfig`) | `ironclaw_llm::circuit_breaker` | **delegated** — types reused so both products report backend health in the same words. The transitions are ours (`ironwire_upstream::breaker`) because ironclaw's live on `LlmProvider`; see below |
| Claude Code credential reading | `ironclaw_llm::anthropic_oauth` (private module) | **ported** into `ironwire_creds::claude` — cannot be delegated until upstream exposes it. Upstream PR: add `CredentialSource::ClaudeCode` |
| Anthropic OAuth header/beta constants, refresh-on-401 shape | `ironclaw_llm::anthropic_oauth` | pattern reused, ~200 LOC |
| Codex `client_version` detection (`codex --version` → `/models?client_version=`) | `ironclaw_llm::codex_chatgpt` (private module) | **not yet done** — gates newer models, so a stale value silently hides models the account is entitled to. Tracked in `ROADMAP.md` |
| Error classification, `retry-after` parsing | `ironclaw_llm::error` | pattern reused in `UpstreamError` |
| Complexity scoring (tier hint only) | `ironclaw_llm::smart_routing` | not built; feature `complexity`, off by default |
| OpenAI Chat/Responses wire types, SSE framing, error mapping | `ironclaw_openai_compat` | not reused — see the note on the native lane below |
| **Whole trace contribution pipeline** — consent policy, deterministic redaction, on-disk queue with holds, manual-review gating, classification, credit/claims, device-key onboarding | `ironclaw_trace_commons` | git dep, feature `contribute`, off by default |

`ironclaw_trace_commons::capture::capture_conversation_trace(scope, messages,
task_failed)` is keyed on a scope string and a plain `ConversationMessage`
list — which is close to exactly what a proxy can synthesize from an observed
exchange. That crate is the entire NEAR-AI-credits half of the product and we
should not reinvent any of it.

**What we deliberately do not reuse:** `ironclaw_llm`'s `LlmProvider` trait and
`CompletionRequest`/`ToolCompletionRequest` types, and any wire-type crate that
implies them. They are a chat-completions-shaped abstraction — exactly the lossy
common denominator the native lane exists to avoid. A typed request struct
cannot express "unchanged", and "unchanged" is the entire fidelity claim
(`tests/passthrough.rs`, `tests/codex_on_subscription.rs`). They are appropriate
for an agent that owns its own prompts; they are not appropriate for a pipe.

That boundary is narrower than it first looks, and an earlier draft of this
document over-applied it. It rules out the *datapath*: the bytes on the wire,
the request and response types, the streaming decode. It says nothing about
credential discovery, price tables, or health vocabulary — all of which are
now delegated rather than reimplemented, per the table above.

---

## 8. Trace ledger

Local capture is **on** by default and stored in
`$IRONWIRE_HOME/ledger.sqlite`; upload is **off** by default.

Recorded per exchange: timestamp, conversation key, the client's own session id
when it sent one, façade, chosen backend + model + rung, requirements, token
usage, cost, TTFT and total latency, finish reason, retry/fallback events,
— behind `capture.logprobs = true` — the confidence aggregates below, and
— behind `capture.bodies = true` — the request and response bodies.

The session id is worth separating from the conversation key, because they
answer different questions. The conversation key is a routing-affinity hash
(protocol family, the head of the preamble, the tool list), deliberately stable
across a whole session — and therefore equally stable across two sessions that
share a tool list, and across machines. The session id is what the agent itself
calls the session: `x-claude-code-session-id` from Claude Code, `session-id`
from Codex, both already forwarded untouched. It is what makes a row in
`ironwire log` line up with the session a user is actually looking at. Recorded
only when it is a plain identifier, so nothing a client sends can reach a
terminal as an escape sequence.

The user-facing value comes first (`ironwire log`, `ironwire replay`, cost
attribution, "what did my agent actually send"). Contribution is a separate,
later, explicit decision that hands the same records to
`ironclaw_trace_commons`, which owns redaction and consent. See
[`TRUST.md`](./TRUST.md) §4.

Because IronWire sees consecutive calls in one conversation, the ledger
naturally captures the signal that matters: *model proposed X → tool returned an
error → model repaired it with Y → next call succeeded.* A later opt-in hooks
plugin can add `git diff`, test results and human acceptance to close the loop.

### `capture.logprobs` — per-token confidence, cross-family only

`capture.logprobs = true` asks a Chat Completions backend for the
log-probability of each token it generates. `false`, the default, does not ask.
It needs `capture.enabled` as well: with no ledger there is nothing to write the
result into, and asking would be per-turn cost with no reader.

The point is a signal the transcript does not carry: *where the model was
uncertain*. A trace's value is not that it is unlike the others; it is that a
human's judgement resolved something the model was unsure about, and entropy at
a decision point is the only thing here that localises that.

**One wire, and that is a fact about the wires rather than a limitation of the
implementation.** Chat Completions has `logprobs: true`. Anthropic Messages has
no such parameter at all and answers an unknown field with a 400 — on every
request, not on some of them. Responses spells it as `top_logprobs` plus an
`include` entry, and has no boolean form. So the pivot IR carries the intent
(`Params::logprobs`) and exactly one emitter honours it; the other two drop it,
which is the one place a silent drop is correct rather than a bug.

**`logprobs`, never `top_logprobs`.** The alternatives are tokens the model
considered and the user never saw. Nothing reads them: the confidence
reduction below is defined over the chosen token. Asking for them would be bandwidth on every
frame and — because `capture.bodies = true` records response bodies verbatim —
a way to persist continuations that were never generated into text. The two
flags together would compose into exactly that, so the alternatives are not
requested at all rather than requested and dropped.

**Cross-family only.** Rule 1 says the native lane forwards bytes;
`docs/PROTOCOL.md` §2 enumerates the mutations and
`crates/ironwire_proxy/tests/passthrough.rs` pins them. The setting is applied
in one place — `pipeline::translate_request`, on the path that already builds a
fresh body — so a request that is not translated is never modified to carry it,
at any setting. A client that asked for log-probabilities itself keeps them
either way: the setting ORs into the parsed request rather than replacing it.

Three reasons it is off by default, any one sufficient:

- It changes what the provider is asked to produce, so a captured exchange is
  **not comparable** to an uncaptured one. The same non-comparability the
  privacy filter records per exchange ([`PRIVACY.md`](./PRIVACY.md) §3).
- It inflates every response materially — on an agent loop that is per-turn
  bandwidth and storage, not a one-off.
- The distributions are conditioned on the whole context, which makes them more
  sensitive than the text they describe.

That last point is where this feature and the privacy filter interact, and the
interaction is favourable. Log-probabilities describe whatever the provider
actually saw. Because substitution happens on the way **out**, before the
request is sent, an upstream running under the filter generates conditioned on
placeholders — so the numbers describe the substituted text, and there was
never an unsubstituted generation to leak. Redaction applied *after* generation
could not make that claim: it cannot scrub numbers produced before it ran. It
is still bounded by the filter's false-negative rate, which
[`PRIVACY.md`](./PRIVACY.md) §7 is explicit cannot be measured on real user
data — so this reduces the exposure rather than removing it, and no interface
may say otherwise.

### Confidence aggregates

The raw distributions never leave the machine and are never written to the
ledger. `ironwire_core::confidence` reduces them, at the end of the exchange, to
four numbers — mean p(chosen token), its population standard deviation across
tokens, a coarse bucket, and the token count the mean is over — and those are
what the ledger records, in `mean_confidence`, `confidence_variability`,
`confidence_bucket` and `confidence_tokens` — four flat, queryable columns,
added through the same additive migration as every other late column, so an
older ledger file keeps working and an older IronWire keeps reading it. Four aggregates give an attacker
essentially nothing to invert; a per-token distribution gives them a great deal.

The vocabulary is borrowed from Swayamdipta et al., *Dataset Cartography*
(2020), and the borrowing is inexact in a way the module documents rather than
papers over: cartography measures p(gold) across training epochs, and
single-pass generation has neither epochs nor a gold label. `variability` here
is dispersion across *tokens*, which differs in kind rather than degree. The
names match the Trace Commons envelope fields they feed, which is the only
reason to keep them.

Both paths are covered. Streaming accumulates in the pivot stream parser as
frames arrive; a non-streaming answer is read out of the buffered body in
`pipeline::translated_body`. The original version of this feature captured only
on the streaming path, which meant a `stream: false` request paid for the
inflated response and recorded nothing — a cost with no signal is the worst of
both.

---

## 9. Failure semantics

| Situation | Behavior |
|---|---|
| Upstream 429 before first byte | try next rung; if none, return the provider's own 429 with `retry-after` intact |
| Upstream 429 after first byte | terminate the SSE with a protocol-correct `error` event. **No transparent retry** — replaying would corrupt the stream |
| Upstream 5xx before first byte | retry same backend (bounded, jittered), then descend |
| Client disconnects | **abort the upstream request** — abandoned requests must not burn quota |
| Credential expired | refresh once inline; on failure mark backend `NeedsAuth`, descend, and surface it in `status` |
| No eligible backend | protocol-correct error the client already handles; never a made-up success |
| Daemon not running | CLI says so and offers `ironwire serve`; never silently proxies to the real provider |

---

## 10. Install surface

Target: `brew`, `npx`, `apt`, `pip`, plus a one-line shell installer. All five
ship the same prebuilt binary; see [`PACKAGING.md`](./PACKAGING.md).

Connecting a client is a command, not a docs page:

```bash
ironwire connect claude   # writes ANTHROPIC_BASE_URL into the user's shell profile / settings.json
ironwire connect codex    # writes the model_provider block into ~/.codex/config.toml
ironwire connect near     # device-key enrollment
ironwire connect openai-api / anthropic-api
ironwire status
ironwire doctor           # verifies each connection end to end with a real 1-token call
```

Every `connect` is reversible (`ironwire disconnect <client>`) and prints the
exact file it is about to modify before modifying it.

---

## 11. Non-goals for v1

- Hosted/multi-user mode. Not "later" — see TRUST.md.
- Owning conversation state, compaction, or tool execution.
- Being a general OpenRouter competitor (arbitrary third-party providers). BYO
  OpenAI-compatible endpoints work, but the product is about *the user's own*
  capacity.
- Training-data collection as a load-bearing business assumption.
