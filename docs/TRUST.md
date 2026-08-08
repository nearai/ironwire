# Trust, credentials and consent

IronWire sits at the point where every byte a developer's coding agent sends —
proprietary source, secrets in tracebacks, customer data in test fixtures — and
every credential that pays for it, pass through one process. This document is
the set of promises that position requires. They are **design invariants**, not
defaults: changing one is a product decision, not a config change.

---

## 1. Structural invariants

| # | Invariant | Enforcement |
|---|---|---|
| I1 | **Loopback only.** IronWire binds `127.0.0.1` and has no remote mode, no `--host`, no tunnel helper. | Hardcoded bind address; test asserts the listener is loopback. |
| I2 | **Credentials never leave the machine.** No credential, refresh token or derived bearer is written to any network destination other than the credential's own issuer. | Egress allowlist keyed by credential; test asserts a credential is only ever attached to its issuer's host. |
| I3 | **No hosted IronWire holds anyone's subscription token.** There is no server-side product that proxies a user's subscription. | Non-goal, stated in DESIGN.md §11. |
| I4 | **Single user.** No multi-tenant routing, no shared pools, no capacity resale. | No tenancy concept exists in the type system. |
| I5 | **No identity forgery.** IronWire never synthesizes another product's client identity to unlock a subscription. | See §3. |
| I6 | **Nothing is uploaded without a recorded, specific consent.** | See §4. |

I1–I4 are the line between *using capacity the user already pays for, on the
user's own machine, for the workload it was sold for* and *reselling
subscription capacity*. That line is not one we approach.

---

## 2. Subscription credential replay

IronWire reads credentials the official clients already store:

| Backend | Source | Auth |
|---|---|---|
| Claude subscription | macOS Keychain `Claude Code-credentials`, else `~/.claude/.credentials.json` | `Authorization: Bearer` + `anthropic-beta: oauth-2025-04-20` |
| ChatGPT/Codex subscription | `~/.codex/auth.json` (`auth_mode: "chatgpt"`) | `Authorization: Bearer`, `chatgpt.com/backend-api/codex` |

Both are undocumented, private surfaces. They can change without notice, and
using them from a third-party process may fall outside what the provider
intends. **The account at risk is the user's.** That fact must be presented
before the first request, not buried.

### Consent gate

Each subscription backend is **off until explicitly enabled**, once, in plain
language:

```
$ ironwire connect claude --subscription

  IronWire will read the OAuth token that Claude Code stores on this machine
  and send requests to api.anthropic.com with it, from this computer only.

  · This uses a private authentication path. Anthropic does not document it
    and may change or block it at any time.
  · Using it from a third-party proxy may fall outside your subscription's
    intended use. If Anthropic objects, it is your account that is affected.
  · Your token is never sent anywhere except api.anthropic.com.
  · You can use an Anthropic API key instead — fully supported, no ambiguity.

  Enable the Claude subscription backend? [y/N]
```

The answer is recorded with a timestamp and the exact prompt version in
`$IRONWIRE_HOME/consent.json`. `ironwire status` always shows which backends are
running on subscription credentials. `ironwire disconnect claude --subscription`
revokes it.

`ironclaw_llm` emits a `tracing::warn!` at this point. A warning in a log is not
consent; this is.

---

## 3. No identity forgery (I5)

Anthropic's OAuth path expects Claude Code's identifying first system block.
That creates a fork with only one acceptable branch:

- ❌ **Inject it for every client.** Aider hitting our Anthropic façade would be
  dressed up as Claude Code to unlock the subscription. This is impersonation of
  another product to a provider, and no framing makes it not that.
- ✅ **Require it.** The subscription backend is eligible only for requests that
  *already* carry that identity — i.e. actual Claude Code. Everything else
  routes to an API key, NEAR AI, or a local model.

The same rule applies to Codex and the ChatGPT backend.

This is a hard eligibility rule in `ironwire_core::policy`, not a default.
It costs us the "point Aider at localhost and use your Claude Max" feature. That
feature was never ours to sell.

---

## 4. Traces

### Defaults

- **Local capture: on.** Metadata only — timing, model, route, rung, attempts,
  status, and the token counts the *provider* reported. Bodies require
  `capture.bodies = true`. Visible via `ironwire log`.
- **No fabricated numbers in the ledger either.** An exchange whose usage the
  provider never reported is stored as unknown and rendered as `—`, not as
  zero: a fabricated zero would silently understate what the user spent.
- **Upload: off.** Not "off until onboarding asks"; off until the user runs an
  explicit command, per scope.

### Why local-first is the right order

The trace ledger has to be worth having for a user who will *never* share
anything: `ironwire log`, `ironwire replay`, per-project cost attribution, "what
did my agent actually send before it did that". If the feature only pays off
when uploaded, the incentive is to nudge users into uploading, and that is how
trust in this position gets spent.

### Upload path

Contribution is delegated wholesale to `ironclaw_trace_commons`, which already
owns:

- deterministic redaction before anything leaves the process (its
  security-critical obligation, with tests pinning the *absence* of raw values),
- standing consent policy and preflight accept/reject,
- an on-disk queue with manual-review holds for high residual-PII-risk
  envelopes,
- classification, allowed-uses and retention labelling,
- credit claims and device-key onboarding.

IronWire does not implement its own redaction. Adding a field to an uploaded
envelope is a change in that crate, under its rules.

### The training question, stated plainly

Both major providers' terms restrict using their model outputs to train
competing models. Traces of a user's own subscription sessions, uploaded by that
user's explicit choice, are the user's decision and the user's terms to honor.

IronWire's design does not depend on that reading holding. The trace corpus is
justified first by evaluation, routing quality and user-facing analytics. If the
training use turns out to be unavailable, the product still works and the ledger
is still worth having. That is deliberate: a product whose economics *require* a
contested legal reading is a product with a single point of failure that
engineering cannot fix.

---

## 5. Handling of secrets in the process

- Tokens are held in `secrecy::SecretString`; no `Debug` impl exposes them.
- No credential is ever written to a log, a trace record, or the control API —
  `status` reports *which* credential is in use, never its value.
- The control API is loopback-only and additionally requires a token from
  `$IRONWIRE_HOME/control.token` (mode 0600), so another local user cannot read
  the ledger or drive routing.
- Request/response bodies, when captured, are stored under `$IRONWIRE_HOME` with
  0700 directory permissions, and are excluded from any `ironwire report`
  bundle unless `--include-bodies` is passed.

---

## 6. What we ask providers for

The end state is explicit support: a sanctioned way for a user to authorize a
local router against their own subscription, with a documented endpoint and a
client identity of its own. Until then IronWire is careful, local, honest with
the user about the risk, and does not build a business on someone else's
ambiguity.
