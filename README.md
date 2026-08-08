# IronWire

**One local inference endpoint for all your AI capacity.**

You are probably paying for Claude Max, ChatGPT Pro, Copilot and a couple of API
accounts — and every one of them is an isolated pool that runs out
independently, in the middle of your work.

IronWire is a loopback proxy that sits at the inference boundary. Point Claude
Code, Codex, or any AI tool at `127.0.0.1`, and IronWire keeps it on the best
available model and degrades gracefully when limits hit.

> **Your coding agent never dies at the rate limit.**

It also stops dying at *"API Error: Response stalled mid-stream."* IronWire
keeps a thinking upstream alive with heartbeats, restarts a stream that failed
before producing any text, rides out a 529 instead of moving you to a metered
key — and when a response really does die, ends it with a stated error rather
than a dropped connection you have to guess about.

```
Claude Code ──┐
Codex ────────┤
Cline ────────┼──► IronWire (127.0.0.1:8463) ──► Claude sub · ChatGPT sub
Aider ────────┤                                   API keys · NEAR AI · local
custom ───────┘
```

Your agent keeps its own loop, context, compaction and tool execution. IronWire
only decides where each conversation's inference goes.

---

## Status

**M1 complete at the wire level; M2 in progress.** Claude Code → IronWire →
Claude subscription works end to end, with an Anthropic API key as fallback,
byte-identical streaming passthrough, observed quota, consent-gated subscription
access, a local trace ledger, and cancellation that provably stops the upstream.

Not yet done: a real multi-hour Claude Code session against a live account.
Until that runs, "no observable behavioural difference" is an inference from
wire-level tests rather than an observation — `scripts/acceptance.sh` is the
check, and [`docs/ROADMAP.md`](docs/ROADMAP.md) tracks it.

## Quick start

```bash
cargo build --release

ironwire connect claude --subscription   # explains the tradeoffs, then asks
ironwire serve                           # in another terminal

export ANTHROPIC_BASE_URL=http://127.0.0.1:8463/anthropic
claude
```

Then:

```bash
ironwire status     # capacity, as the providers reported it
ironwire log        # what your agents sent, and what it cost
ironwire doctor     # probe every backend for real
```

```
IronWire 0.1.0 — http://127.0.0.1:8463

Claude subscription
  connected · subscription
  capacity: [████████░░] 82% used · resets in 2h14m · observed 40s ago
  models: claude-opus-4-6, claude-sonnet-4-6, claude-haiku-4-5

Anthropic API
  connected · api key
  capacity: unknown (the provider has not reported yet)

1 conversation(s) with a sticky route
```

Every number there came from the provider. Where nothing was reported, IronWire
says `unknown` rather than showing a plausible guess — see
[`docs/CRITIQUE.md`](docs/CRITIQUE.md) §4 for why that matters.

## How it decides

Routing is **per conversation, not per request.** A coding session carries a
large warm prompt cache and often provider-private reasoning state; moving it
costs money and latency, and moving it across API families can cost correctness.
So fallback is a state transition taken under sustained pressure, down an
explicit ladder:

| Rung | Route | Cache | Reasoning | Told? |
|---|---|---|---|---|
| 0 | same account, same model | warm | intact | — |
| 1 | same account, smaller model | warm-ish | intact | — |
| 2 | same wire format, different credential | cold | intact | — |
| 3 | different API family | cold | dropped | **yes** |

Rungs 0–2 need no translation at all — IronWire forwards your request's bytes
untouched. Rung 3 does: a Claude Code session can keep working on NEAR AI (or
any OpenAI-compatible endpoint) when your Anthropic capacity runs out.

The rule that makes rung 3 safe is **switch families at a turn boundary, never
mid tool loop** — a conversation caught mid-loop waits one turn rather than
being refused. Everything that would genuinely *break* the request (tools a
backend does not have, images it cannot see, a context that does not fit) is
still refused outright rather than silently degraded. See
[`docs/PROTOCOL.md`](docs/PROTOCOL.md) §6.

## What IronWire promises about your credentials

IronWire sits where all of your source code and all of your API credentials
pass. That position comes with hard commitments, not defaults
([`docs/TRUST.md`](docs/TRUST.md)):

- **Loopback only.** Binds `127.0.0.1`. No remote mode, no `--host`, no tunnel.
- **Credentials never leave this machine**, and are only ever attached to the
  host that issued them.
- **No hosted IronWire holds anyone's subscription token.** Not planned, not
  later.
- **No identity forgery.** IronWire will not present one product's client
  identity to unlock another product's subscription. That means Aider pointed at
  the Anthropic façade routes to an API key, not to your Claude Max.
- **Subscription backends are off until you say yes**, once, to a specific
  question that names the risk and whose account bears it.
- **Traces stay local by default.** Upload is a separate, explicit decision.
- **IronWire never updates itself.** `ironwire update` tells you a release
  exists and prints your package manager's command. The only thing it refreshes
  on its own is a signed provider-quirks document — which, by construction,
  cannot name a host, so it can never redirect where a credential goes.

## Documentation

| | |
|---|---|
| [`docs/DESIGN.md`](docs/DESIGN.md) | The architecture |
| [`docs/CRITIQUE.md`](docs/CRITIQUE.md) | The design review that produced it |
| [`docs/PROTOCOL.md`](docs/PROTOCOL.md) | Wire fidelity: what is mutated, what is refused |
| [`docs/TRUST.md`](docs/TRUST.md) | Credentials, consent, traces |
| [`docs/PRIVACY.md`](docs/PRIVACY.md) | The optional privacy filter — designed, not built |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | Milestones |
| [`docs/PACKAGING.md`](docs/PACKAGING.md) | brew / npx / apt / pip |
| [`docs/UPDATES.md`](docs/UPDATES.md) | notify-only updates, and the signed quirks channel |

## Layout

```
crates/
  ironwire_core       protocols, capability gate, routing policy, quota
  ironwire_creds      credential discovery + consent
  ironwire_ledger     the local trace ledger
  ironwire_quirks     the signed provider-quirks channel
  ironwire_update     notify-only update checking
  ironwire_translate  cross-family translation (the fallback lane)
  ironwire_upstream   backends: native passthrough and observation
  ironwire_proxy      axum façades, pipeline, control API
  ironwire_cli        the `ironwire` binary
```

## Development

```bash
cargo test              # unit + conformance
cargo clippy --all-targets
```

Three suites carry the fidelity claim:

| Suite | Proves |
|---|---|
| `tests/passthrough.rs` | request and response bytes are identical to the originals, modulo the mutations `docs/PROTOCOL.md` §2 enumerates |
| `tests/multi_turn.rs` | a three-turn tool loop — signed thinking, replayed tool ids, cache breakpoints — survives, and stays on one backend |
| `tests/cancellation.rs` | an abandoned request stops the upstream, and still records what it consumed |
| `tests/claude_code_on_nearai.rs` | a Claude Code session keeps working on NEAR AI when Anthropic capacity is exhausted — and waits for a turn boundary rather than switching mid tool loop |
| `tests/stalled_stream.rs` | the "stalled mid-stream" failures: a thinking upstream is kept alive, a thinking-gap failure is restarted invisibly, a post-content failure is reported rather than replayed, a 529 is retried |

`scripts/acceptance.sh` is the manual check the mocks cannot replace: a real
Claude Code task, through IronWire, against real providers. It costs
subscription quota — run it before a release, not in CI.

## License

MIT OR Apache-2.0.
