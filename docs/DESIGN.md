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

### Translated lane — fallback only

Inbound protocol != backend protocol. IR-mediated, and **capability-gated: a
route that cannot preserve the request's semantics is ineligible, not
best-effort.**

```rust
/// A route is eligible only if every requirement the request carries is
/// preserved by the target. Anything else is a refusal, not a downgrade.
struct RequestRequirements {
    tools: bool,
    parallel_tool_calls: bool,
    images: bool,
    reasoning: ReasoningNeed,     // None | Requested | LoadBearing (signed blocks present)
    prompt_cache: bool,           // cache_control breakpoints present
    structured_output: bool,      // strict JSON schema
    min_context_tokens: u32,
    continuation: Option<Continuation>, // prior signed/encrypted state in-flight
}
```

`ReasoningNeed::LoadBearing` and `Continuation::Some` make **every** cross-family
route ineligible, permanently, for that conversation. That is not a limitation
to engineer away — it is a property of signed and encrypted provider state.

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
  ironwire_core       types, config, capability registry, policy, quota ledger
  ironwire_creds      credential discovery + refresh (Claude Code, Codex, keys)
  ironwire_upstream   Backend trait; native passthrough clients; observation
  ironwire_proxy      axum façades, router wiring, control API, trace sink
  ironwire_cli        the `ironwire` binary
```

Dependency direction is strictly downward; `core` depends on nothing of ours.

### The daemon and its control API

One daemon per machine, lockfile at `$IRONWIRE_HOME/daemon.lock`, listening on
`127.0.0.1:8463`. Everything except the two façade prefixes lives under
`/_ironwire/`:

```
GET  /_ironwire/status          full state snapshot (JSON)
GET  /_ironwire/events          SSE: route decisions, quota changes, errors
GET  /_ironwire/backends
POST /_ironwire/pin             { model | backend | clear }
GET  /_ironwire/log?limit=      recent exchanges from the local ledger
POST /_ironwire/shutdown
```

`ironwire status` and the macOS menu bar app are both **clients of this API**.
No routing logic lives in the CLI or in Swift; otherwise they diverge.

---

## 7. Reuse of `nearai/ironclaw`

Reuse is deliberate and layered, because binary size is a distribution
constraint (`brew`/`npx`/`apt`/`pip` all want one small self-contained
executable) and `ironclaw_llm` pulls `rig-core`.

| What | From | How |
|---|---|---|
| Codex credential file format, refresh, ChatGPT base URL | `ironclaw_llm::auth` (`CredentialSource::CodexCli`) | git dep, feature `ironclaw-auth` |
| Claude Code credential reading | `ironclaw_llm::anthropic_oauth` (currently private) | **ported into `ironwire_creds`**; upstream PR to add `CredentialSource::ClaudeCode` |
| Anthropic OAuth header/beta constants, refresh-on-401 shape | `ironclaw_llm::anthropic_oauth` | pattern reused, ~200 LOC |
| Error classification, `retry-after` parsing, rate-limit detection | `ironclaw_llm::error` | pattern reused |
| Retry / circuit breaker / failover / cooldown semantics | `ironclaw_llm::{retry, circuit_breaker, failover}` | pattern reused, adapted to before-first-byte-only |
| Price table | `ironclaw_common::llm_costs` | git dep (light: no rig) |
| Complexity scoring (tier hint only) | `ironclaw_llm::smart_routing` | feature `complexity`, off by default |
| OpenAI Chat/Responses wire types, SSE framing, error mapping | `ironclaw_openai_compat` (`chat.rs`, `responses.rs`, `content_parts.rs`, `streaming.rs`, `error.rs`) | extracted into `ironwire_wire_openai`; the workflow half is ironclaw-specific and not reused |
| **Whole trace contribution pipeline** — consent policy, deterministic redaction, on-disk queue with holds, manual-review gating, classification, credit/claims, device-key onboarding | `ironclaw_trace_commons` | git dep, feature `contribute`, off by default |

`ironclaw_trace_commons::capture::capture_conversation_trace(scope, messages,
task_failed)` is keyed on a scope string and a plain `ConversationMessage`
list — which is close to exactly what a proxy can synthesize from an observed
exchange. That crate is the entire NEAR-AI-credits half of the product and we
should not reinvent any of it.

**What we deliberately do not reuse:** `ironclaw_llm`'s `LlmProvider` trait and
`CompletionRequest`/`ToolCompletionRequest` types. They are a
chat-completions-shaped abstraction — exactly the lossy common denominator the
native lane exists to avoid. They are appropriate for an agent that owns its
own prompts; they are not appropriate for a pipe.

---

## 8. Trace ledger

Local capture is **on** by default and stored in
`$IRONWIRE_HOME/ledger.sqlite`; upload is **off** by default.

Recorded per exchange: timestamp, conversation key, façade, chosen backend +
model + rung, requirements, token usage, cost, TTFT and total latency, finish
reason, retry/fallback events, and — behind `capture.bodies = true` — the
request and response bodies.

The user-facing value comes first (`ironwire log`, `ironwire replay`, cost
attribution, "what did my agent actually send"). Contribution is a separate,
later, explicit decision that hands the same records to
`ironclaw_trace_commons`, which owns redaction and consent. See
[`TRUST.md`](./TRUST.md) §4.

Because IronWire sees consecutive calls in one conversation, the ledger
naturally captures the signal that matters: *model proposed X → tool returned an
error → model repaired it with Y → next call succeeded.* A later opt-in hooks
plugin can add `git diff`, test results and human acceptance to close the loop.

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
