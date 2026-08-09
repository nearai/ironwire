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

**Built and tested at the wire level. Not yet run against a live
subscription.** That distinction is the whole of this section, so it is stated
before anything else.

Working, with tests that pin each claim: both native lanes (Claude Code →
Anthropic, Codex → ChatGPT/OpenAI), the translated lane to NEAR AI, byte-identical
passthrough, observed quota, consent-gated subscription access, per-backend
circuit breaking, stream resilience, compaction-aware routing, a local trace
ledger with real costs, and an optional privacy filter.

**Every one of those is verified against a mock.** A live Claude or ChatGPT
subscription would exercise header sets and rate-limit shapes that no fixture
can prove. `docs/ROADMAP.md` has a table of exactly which items need a real
account, a signing key, or hosting — none of them are "not done yet", and
listing them keeps them from reading as finished.

## Install

```bash
curl -fsSL https://ironwire.dev/install.sh | sh   # any Unix
brew install nearai/tap/ironwire                  # macOS, Linux
npx ironwire@latest                               # no install
pip install ironwire                              # the Aider crowd
```

Or from source: `cargo install --git https://github.com/nearai/ironwire`.

> The hosted channels above need a published release. Until one exists, build
> from source — `docs/ROADMAP.md` lists what each channel is waiting on.

## Quick start

```bash
ironwire init       # what capacity this machine has, and what to run next
```

`init` reads the room — a Claude Code login, a Codex login, API keys in your
environment — and prints the steps in order. Roughly:

```bash
ironwire connect claude --subscription   # explains the tradeoffs, then asks
ironwire serve                           # leave this running

# in another terminal
eval "$(ironwire env)"                   # points Claude Code here
ironwire doctor                          # confirms it actually is
claude
```

`doctor` checks the *clients*, not just the backends. Every backend can be
healthy while your agent still goes straight to the provider because nothing
points it here — the commonest way this looks broken when it is not.

Then:

```bash
ironwire status     # capacity, as the providers reported it
ironwire log        # what your agents sent, and what it cost
ironwire watch      # live routing; silent unless something changes
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

## How it tells you

IronWire will not write into your agent's transcript. The only channel there is
the response stream, and a line in it would be a line the model appears to have
said. So it uses the places a harness keeps for something other than the model:

```bash
ironwire connect claude    # also offers to install the status line
ironwire statusline        # what it prints there, if you want to see it
```

In Claude Code that is the status bar, via the `statusLine` hook — which
IronWire fills only if you have not written your own, and removes on
`ironwire disconnect claude`. It stays quiet: where traffic is going, a
substituted model, a pool above a third used, a fallback for the ten minutes
after it happens, and a release when one exists.

Codex has no status-line hook (`tui.status_line` exists in its config and
renders nothing in 0.145), but it does render one line from the server on a
usage-limit response. That is where IronWire says whether it had anywhere else
to go — the moment you are stopped is the moment that matters.

`ironwire watch` is still there for a second terminal, and is still the only
place that shows every decision as it is made.

## The optional privacy filter

Off by default. When you turn it on, IronWire substitutes sensitive values on
the way out and restores them on the way back, so a provider never sees them
and you never see a placeholder.

```bash
ironwire privacy check src/config.rs   # what it would catch, before you rely on it
```

Two things it deliberately does not do. It does not claim to make anything
*safe* — it has a false-negative rate nobody can measure on your data, so
`privacy check` reports what it found and never says "clean"
([`docs/TRUST.md`](docs/TRUST.md) I7). And it does not substitute values inside
code blocks, tool results, or documentation ranges like `example.com` and
RFC 1918 addresses: in a coding session those are load-bearing *code*, and
replacing them makes the model write something that does not work.

[`docs/PRIVACY.md`](docs/PRIVACY.md) is the design and the critique of it,
including where the mechanism stops.

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
| [`docs/PRIVACY.md`](docs/PRIVACY.md) | The optional privacy filter, and where it stops |
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
  ironwire_privacy    reversible substitution (the optional filter)
  ironwire_translate  cross-family translation (the fallback lane)
  ironwire_upstream   backends: native passthrough and observation
  ironwire_proxy      axum façades, pipeline, control API
src/
  the `ironwire` binary — the workspace root package, so `cargo run` and
  `cargo install --path .` work from a fresh clone
```

## Development

```bash
cargo test              # unit + conformance
cargo clippy --all-targets
```

These suites carry the claims the design rests on:

| Suite | Proves |
|---|---|
| `tests/passthrough.rs` | request and response bytes are identical to the originals, modulo the mutations `docs/PROTOCOL.md` §2 enumerates |
| `tests/multi_turn.rs` | a three-turn tool loop — signed thinking, replayed tool ids, cache breakpoints — survives, and stays on one backend |
| `tests/cancellation.rs` | an abandoned request stops the upstream, and still records what it consumed |
| `tests/claude_code_on_nearai.rs` | a Claude Code session keeps working on NEAR AI when Anthropic capacity is exhausted — and waits for a turn boundary rather than switching mid tool loop |
| `tests/codex_on_subscription.rs` | Codex reaches ChatGPT byte-identical, and a non-Codex client is never merely refused but never dialled |
| `tests/stalled_stream.rs` | the "stalled mid-stream" failures: a thinking upstream is kept alive, a thinking-gap failure is restarted invisibly, a post-content failure is reported rather than replayed, a 529 is retried |
| `tests/circuit.rs` | a dead backend stops being dialled — but the last one standing is still tried, because a breaker should waste less time, not turn a degraded proxy into a dead one |
| `tests/privacy_filter.rs` | with the filter on, the provider never sees the value and the client never sees a placeholder |
| `tests/privacy_compaction.rs` | the same, across a compaction boundary on all three wires — where a mistake becomes permanent history |

Two scripts run the parts a unit test cannot see:

```bash
scripts/journey.sh          # the commands a person runs, in order, against a mock
scripts/test-install.sh     # install.sh, including its failure paths
scripts/test-packaging.sh   # the release scripts, before a tag depends on them
```

`scripts/journey.sh` is where every integration bug in this project has been
found — including a privacy-filter path that bypassed reversal on reconnect,
which every unit test was happy about.

`scripts/acceptance.sh` is the one check the mocks cannot replace: a real
Claude Code task, through IronWire, against real providers. It costs
subscription quota — run it before a release, not in CI.

## License

MIT OR Apache-2.0.
