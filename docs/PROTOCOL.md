# Protocol fidelity

The native lane's promise is that a request is indistinguishable, upstream, from
one the client sent directly — and that the response is indistinguishable,
downstream, from one the provider sent directly. This document is what that
costs.

---

## 1. Façade surface

### Anthropic façade — `http://127.0.0.1:8463/anthropic`

Set `ANTHROPIC_BASE_URL` to that URL. Claude Code appends `/v1/...` itself.

| Route | Required by | Notes |
|---|---|---|
| `POST /v1/messages` | everything | streaming and non-streaming |
| `POST /v1/messages/count_tokens` | **Claude Code** | drives its context budget and compaction trigger. A 404 here silently breaks context accounting. |
| `GET /v1/models` | model pickers | synthesized from the backends that are actually eligible |

Headers that must survive untouched: `anthropic-version`, `anthropic-beta` (all
values, comma-joined), `x-api-key`/`authorization` (replaced, not forwarded),
and any `anthropic-*` we do not recognize.

### OpenAI façade — `http://127.0.0.1:8463/openai`

| Route | Wire API | Used by |
|---|---|---|
| `POST /v1/responses` | Responses | Codex (`wire_api = "responses"`) |
| `POST /v1/chat/completions` | Chat Completions | Aider, Cline, most third parties |
| `GET /v1/models` | — | pickers |

---

## 2. What "passthrough" means precisely

For a native-lane request, IronWire performs exactly these mutations:

1. **URL**: façade path → backend base URL + same suffix.
2. **Auth headers**: strip inbound `authorization` / `x-api-key`; attach the
   chosen backend's credential.
3. **Host-ish headers**: strip hop-by-hop headers (`connection`,
   `transfer-encoding`, `keep-alive`, `upgrade`, `proxy-*`), set `host`.
4. **`model`**: rewritten *only* when policy selected a different model, via a
   targeted JSON edit of that one key — never a full re-serialize.
5. **Nothing else.** The body is otherwise the bytes the client sent.

> **The one documented exception, and it is opt-in.** The privacy filter
> (`docs/PRIVACY.md`) substitutes values on the way out and restores them on the
> way back. That is a mutation of both the body and the response stream, so it
> is off by default, announced in `ironwire status` while it is on, and marked
> per exchange in the ledger. This enumeration — and `tests/passthrough.rs` —
> describe the filter-off path, which is the only path that makes the
> byte-identity claim.

Everything IronWire needs for policy comes from a **peek**: a bounded,
non-consuming scan of the parsed body for `model`, `stream`, `system[0]`,
message count, `tools[].name`, and the presence of `cache_control`, `thinking`,
image blocks and `reasoning`. The peek result never re-encodes the body.

### Streaming

The response body is forwarded as a byte stream, not re-framed. A tee reads a
copy for observation. Consequences we accept:

- Backpressure is the client's; a slow reader slows the upstream read.
- The observation tee must never be able to stall or fail the forward path. It
  runs on a bounded channel and **drops observations under pressure** rather
  than blocking bytes.
- `text/event-stream` responses set `Cache-Control: no-cache`,
  `X-Accel-Buffering: no`, and no compression is negotiated on our hop.

---

## 3. Observation

Usage and quota come off the wire, never from a model of our own (DESIGN §4).

**Anthropic (SSE):**
- `message_start` → `message.usage` (input, cache creation, cache read tokens)
- `message_delta` → `usage.output_tokens` (cumulative)
- `message_stop` → finalize
- headers: `anthropic-ratelimit-unified-*`, `retry-after`

**OpenAI Responses (SSE):**
- `response.completed` → `response.usage`
- headers: rate-limit windows returned by the ChatGPT backend

**Anthropic/OpenAI non-streaming:** `usage` object in the body.

An exchange whose usage could not be observed is recorded with
`usage: unknown`, not with an estimate.

---

## 4. Cancellation

Client disconnect must abort upstream. In axum this means holding the upstream
request's `AbortHandle` in a guard tied to the response body's `Drop`. Abandoned
requests otherwise keep generating — burning exactly the scarce quota IronWire
exists to protect.

Proved by `crates/ironwire_proxy/tests/cancellation.rs`: an upstream that streams
forever, a client that reads two frames and walks away, and an assertion that the
upstream's next writes fail. The same test pins the other half — the observation
tee flushes on `Drop`, so an abandoned request still records the tokens the
provider had already reported. Cancellation that loses the accounting would hide
exactly the spend the user needs to see.

---

## 5. Retry, failover, and keeping a stream alive

### The point of no return is the first *content* byte

An earlier revision of this section said retry ends at the first byte of the
response body. That was too strict, and it gave away the most recoverable
failure there is.

`message_start` and `ping` carry no content — they are an envelope and a
heartbeat. A stream that has emitted only those has told the client nothing it
would later have to un-tell. Anthropic's shape is `message_start`, then often a
long silence while the model thinks, then content; failures land in that gap
constantly, and under the old rule every one of them was unrecoverable for no
reason.

So the boundary moved:

```
request received
   │
   ├─ [retryable] connect · send · response head · message_start · ping
   │      429 / 5xx / connect error / reset / silence
   │      → retry the same backend, or descend the ladder
   │      `message_start` is HELD, so a restart is invisible
   │
   ├─ FIRST CONTENT EVENT ──────────── point of no return
   │
   └─ [committed] any failure becomes a terminal `error` event on the open
          stream. Never retried: replaying would duplicate text the client has
          already written into its transcript.
```

`ironwire_proxy::resilience` implements this; `tests/stalled_stream.rs` pins it.

### Keeping a quiet stream alive

The other half of "Response stalled mid-stream" is not a failure at all — the
upstream is thinking, and the client's patience runs out first. IronWire emits
`ping` events after `resilience.keepalive_secs` of upstream silence. A ping is
part of the Anthropic event stream, carries nothing, and states something true:
still connected, still waiting.

Two rules keep that honest rather than a way of hiding a hang:

- **Upstream pings are forwarded immediately**, never held. A delayed heartbeat
  is not a heartbeat, and holding them would grow the pre-commit buffer without
  bound.
- **We give up too.** After `resilience.stall_timeout_secs` of continuous
  silence the stream ends with a stated `error` rather than pinging into the
  void. A user waiting forever is a worse outcome than a user told to retry.

### Ending a dead stream honestly

When an upstream dies mid-response the old behaviour was to drop the connection,
leaving the client to infer a stall — which is literally the message the user
sees. Now the stream ends with a terminal `error` event in the shape Claude Code
already handles, saying whether the response was truncated and whether retrying
is worth it.

### Same-backend retry before descending

A 529 or a reset is usually momentary. Descending the fidelity ladder over one
would move a warm conversation off its subscription and onto a metered key the
user pays for. So a transient failure retries the **same** backend up to
`MAX_SAME_BACKEND_RETRIES` times with exponential backoff first, and only then
descends.

A 429 that carries a real `retry-after` is the exception: the provider told us
how long, and sleeping through it while other capacity sits idle would stall the
agent for no reason. Those descend immediately.

---

## 6. Where translation is lossy, and what we do about it

### The rule: switch families at a turn boundary, never mid tool loop

This section originally claimed that a signed `thinking` block made a
cross-family route permanently ineligible. **That was wrong**, and the error was
expensive: it deleted an entire capacity pool for the product's flagship use
case, because Claude Code runs with thinking on and therefore carries signed
blocks from turn one.

What actually happens:

| Direction | Behaviour |
|---|---|
| Anthropic thinking blocks → a foreign provider | The provider never validates them. We drop them during translation. Nothing errors. |
| Foreign reasoning state → Anthropic | Anthropic **drops** it from the prompt rather than rejecting it — silently, and unbilled. |
| Anthropic thinking blocks → Anthropic | Must be replayed **unchanged**. Modifying one is rejected; *removing* one can trigger an ordering/signature 400. |

The third row is the whole constraint, and it bites on the **return** path. A
turn served by a foreign provider produces an assistant turn with a `tool_use`
and no thinking block. Replay that to Anthropic mid-loop — where the block is
expected — and you get a 400 the user cannot diagnose. At a turn boundary
(the last message is a fresh user turn, not a replayed `tool_result`) there is
no in-flight state to be missing.

So the gate is **one boolean**, `RequestRequirements::mid_tool_loop`:

```rust
if cross_family && req.mid_tool_loop {
    return Err(Ineligible::MidToolLoop);
}
```

It **defers** a switch to the next clean turn rather than disqualifying the
conversation. In a coding session that is at most one turn of waiting.

The rule is about *switching*, so it does not apply to the backend a
conversation is already on. Continuing on the foreign backend mid-loop is where
that loop's state lives; treating the incumbent as ineligible inverted the rule,
because the conversation then fell through to a fresh selection and was dragged
back to the native family — mid tool loop, which is the move the rule exists to
prevent.

### Climbing back

The ladder is descended *and* climbed. A conversation that fell to rung 3
returns to a better rung once one has been continuously available for
`PROMOTION_DEBOUNCE` — five minutes, against twenty seconds for a descent, and
the asymmetry is deliberate. Descending is urgent, because the alternative is a
failed turn. Promoting is not, because the conversation is working; and
`Headroom::is_pressured` is a step function at 90% with no hysteresis of its
own, so without a long debounce a provider hovering at that line would move the
conversation — discarding a warm prompt cache — every twenty seconds. That is
strictly worse than never promoting.

Leaving a foreign family is gated by exactly the same turn boundary as entering
it, for exactly the same reason: the recent assistant turns carry none of the
native family's signed reasoning state. A blocked promotion waits for a clean
turn without losing its accumulated recovery, so a busy tool loop postpones the
climb rather than cancelling it.

### What crossing a family actually costs

None of these are refusals. They are what the rung-3 announcement is for:

| Lost | Consequence |
|---|---|
| Signed thinking / encrypted reasoning | No reasoning continuity across the switch. Quality drop, no error. |
| Prompt cache | Cold start. Refused only when the warm prefix exceeds `CACHE_SACRIFICE_THRESHOLD_TOKENS` — an economic call, not a semantic one. |
| Client-specific system-prompt tuning | The prompt was written for Claude; another model follows it less precisely. |

### What is still a hard refusal

| Request carries | Why it breaks, not degrades |
|---|---|
| tools, backend has none | The agent waits forever for a call that cannot come |
| **used** parallel tool calls, backend is serial | All but one call is silently dropped |
| images, backend is text-only | The model cannot see the input it is being asked about |
| strict JSON schema, backend cannot guarantee one | The client fails to parse the answer |
| a prompt larger than the context window | — |

Note the second row: the gate fires on *history that already issued several
calls in one turn*, not on the permissive Anthropic default. Reading "permitted"
as "required" would refuse every serial backend for every request — the same
class of over-strictness as the original reasoning rule.

### Tool-call identity

Cross-family routes need `toolu_* ↔ call_*` to survive both directions, because
the client replays whatever id we returned, forever.

The obvious design is a conversation-lifetime map. IronWire deliberately does
**not** use one: a map is state that can be lost (restart, eviction), and losing
it mints *invalid ids* — undiagnosable and unrecoverable. Instead the mapping is
a stateless reversible encoding (`ironwire_translate::tool_ids`): a foreign id
becomes `toolu_xw_<original>`, and the prefix is stripped on the way back. Two
processes agree without sharing anything, and a restart changes nothing.

### Not yet done: switching mid-loop

The turn-boundary rule is the conservative choice, and it leaves capacity on the
table: a session that spends most of its life inside tool loops can wait a while
for a boundary. Lifting it means synthesizing the missing state on the return
path — either an empty/placeholder thinking block on the foreign assistant turn,
or stripping the whole turn's `tool_use`/`tool_result` pair and re-issuing it
natively.

Both need to be validated against the live API before shipping, since the exact
tolerance for a missing block at each position is not documented. Until then,
mid-loop switching is a **known limitation, not a design position**. It is
tracked in `ROADMAP.md` under M3.

### Content this build does not model

The translator handles `text`, `thinking`, `image`, `tool_use` and
`tool_result`. Anything else — a `document`, a `search_result`, whatever
Anthropic ships next — is **named in `Dropped::unknown_blocks` and makes the
cross-family route ineligible.**

Refusing rather than dropping, because the two are indistinguishable from here.
A `document` block a user asked a question about looks exactly like one that was
decorative, and translating it away produces an answer about content the model
never received — silent degradation, which §6 exists to refuse. The native lane
carries it perfectly, so the cost is waiting for same-family capacity.

An earlier version let these fall through a catch-all arm and vanish without
appearing in the dropped report at all, which made this document's own promise
false for exactly the case where it mattered most.

## 7. Conformance testing

Fidelity claims are worthless without a harness that proves them.

1. **Golden-pair corpus.** Capture real Claude Code and Codex sessions
   (`IRONWIRE_RECORD=1`) into fixtures: exact request bytes and exact upstream
   response bytes.
2. **Passthrough identity test.** Replay each fixture through the native lane
   against a mock upstream; assert the bytes the mock received differ from the
   original *only* in the header/URL/model mutations enumerated in §2, and that
   the bytes the client received are byte-identical to the recorded upstream
   response.
3. **Stream-shape test.** Assert SSE event order, event names and frame
   boundaries are preserved exactly.
4. **Live smoke.** `ironwire doctor` hits every connected backend for real and
   reports per-backend latency.

   The probe must not claim another product's identity (`TRUST.md` §3), which
   rules out the obvious design. A synthetic 1-token *message* against the
   Claude subscription would have to carry Claude Code's system preamble to be
   accepted — i.e. IronWire pretending to be Claude Code, which is the one thing
   the architecture refuses. So subscription backends are probed with an
   auth-only call (`GET /v1/models`), which validates the credential and costs
   nothing. Metered backends may use a real 1-token request.

   `doctor` also skips any backend that is authenticated but not consented:
   probing it would use the very credential the user has not authorised.
5. **Cross-family acceptance.** `tests/claude_code_on_nearai.rs` runs a Claude
   Code-shaped session against an exhausted Anthropic backend and a NEAR AI
   stand-in: it asserts the fallback happens, that the client receives a
   well-formed Anthropic stream, that a NEAR AI tool call arrives as a valid
   `tool_use` block whose id round-trips, that a mid-loop conversation does
   **not** switch, and that the same conversation does switch one turn later.
6. **Agent-level acceptance.** A scripted Claude Code task (edit a file, run a
   test, fix the failure) must complete through IronWire with the same
   turn count as direct. This is the only test that catches subtle behavioral
   regressions from a mis-mapped field.

---

## 8. Compaction

Every serious harness compacts: as a conversation approaches the context limit
it asks the model to summarize what has happened so far, then continues from
that summary. IronWire sees the compaction turn as an ordinary request, which
is convenient and also the source of the problem — **it is not an ordinary
request, and treating it as one is wrong in three separate ways.**

| Harness | Trigger | Reaches us as |
|---|---|---|
| Claude Code | auto near the limit, plus `/compact`; the trigger is computed from `POST /v1/messages/count_tokens` | `POST /v1/messages` |
| Codex | auto, plus `/compact` | `POST /v1/responses` |
| Aider | chat-history summarization above a threshold | `POST /v1/chat/completions` |
| Cline / Roo | "Condense context" | whichever façade is configured |

### What makes it different

**Its output is permanent.** An ordinary turn is disposable — a bad answer is
visible and retryable. A compaction summary *becomes the conversation*: it is
written into the client's history and resent on every subsequent turn for the
rest of the session. A degraded summary degrades everything after it, and the
user has no way to tell that is what happened.

**It is the largest request of the session and the slowest.** The whole history
goes up; a long summary comes back after a long think. It is therefore the turn
most likely to hit a stall, an overload, or a read timeout — precisely where
`resilience` (§5) matters most, and precisely where a same-backend retry is
worth more than descending the ladder.

**Our own turn-boundary rule permits a family switch here.** `mid_tool_loop` is
false during compaction — it is a clean boundary by construction. So the
cross-family gate in `capability::eligible` *allows* a switch at the one moment
in the session when a switch is most expensive. The gate is not wrong; it is
answering a different question (is a switch *correct*) than the one that matters
here (is a switch *wise*).

### Consequences

1. **Fidelity should dominate cost on a compaction turn.** Descending a rung to
   save money buys one cheaper request and pays for it on every turn afterwards.
   Not yet implemented — tracked in `ROADMAP.md`.
2. **Recognition must stay optional.** Fingerprinting each harness's compaction
   request is the obvious approach and is exactly the kind of client-shape
   detail that breaks silently on a client update. Any fingerprint belongs in
   the signed quirks channel (`docs/UPDATES.md`), and nothing may depend on it
   being right — behaviour without recognition must be correct, just less
   tuned.
3. **`count_tokens` must keep working and must be routed like the turn it
   predicts.** Claude Code computes its compaction trigger from it (§1). It
   already goes through the same routing so the count comes from the model that
   will serve the turn; if that ever stops being true, the client will compact
   at the wrong moment in both directions.
4. **Affinity survives, and should.** The conversation key is derived from the
   system preamble and tool names (`DESIGN.md` §3), which compaction does not
   change. A compacted conversation keeps its sticky route — which is what we
   want, since the provider-side prompt cache is about to be rebuilt either way.
5. **For the privacy filter, this is the hardest case in the product.** An
   unreversed placeholder in a summary is permanent, self-perpetuating
   corruption of the user's transcript. See `docs/PRIVACY.md` §5.
