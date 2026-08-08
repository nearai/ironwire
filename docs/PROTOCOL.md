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

## 5. Retry and failover boundaries

**The only safe retry point is before the first byte of the response body has
reached the client.**

```
request received
   │
   ├─ [retryable window] connect / send / await response head / first body byte
   │      429, 5xx, connect error, auth error → retry or descend the ladder
   │
   ├─ FIRST BYTE FORWARDED ──────────── point of no return
   │
   └─ [committed] any upstream failure is surfaced as a protocol-correct
          terminal event on the open stream. No transparent retry: replaying
          would duplicate content the client has already committed to its
          transcript.
```

For the Anthropic façade the committed-failure shape is an SSE `error` event
followed by stream close. For Responses, `response.failed`.

---

## 6. Where translation is lossy, and what we do about it

The translated lane refuses rather than degrades. The gate:

| Request carries | Anthropic → Chat Completions | Anthropic → Responses | Responses → Anthropic |
|---|---|---|---|
| tools | ✅ shape-mapped | ✅ | ✅ |
| parallel tool calls | ✅ | ✅ | ✅ |
| images | ✅ (base64 → data URL) | ✅ | ✅ |
| `thinking` requested | ⚠️ maps to `reasoning_effort` | ✅ | ⚠️ |
| **signed `thinking` blocks in history** | ❌ **ineligible** | ❌ **ineligible** | ❌ |
| **encrypted reasoning items in history** | ❌ | ❌ | ❌ **ineligible** |
| `cache_control` breakpoints | ❌ dropped → **ineligible unless prefix < 4k tokens** | ❌ same | ❌ |
| strict JSON schema | ✅ | ✅ | ⚠️ |
| tool IDs | ✅ via session-lifetime bidirectional map | ✅ | ✅ |

❌ entries are hard ineligibility in `ironwire_core::policy::eligible()`. A
conversation that has ever received a signed thinking block is pinned to the
Anthropic family for its lifetime. This is a property of the provider's
cryptography, not a gap in our mapper.

### Tool-call ID mapping

Cross-family routes require a bidirectional `toolu_* ↔ call_*` map that lives as
long as the conversation, because the client replays IDs we minted, forever.
The map is memory-resident, keyed by conversation, and its loss (daemon restart)
downgrades the conversation to rung ≤ 2 rather than producing invalid IDs.

---

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
5. **Agent-level acceptance.** A scripted Claude Code task (edit a file, run a
   test, fix the failure) must complete through IronWire with the same
   turn count as direct. This is the only test that catches subtle behavioral
   regressions from a mis-mapped field.
