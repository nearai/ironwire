# Hosting IronWire in another application

`ironwire_proxy::embed::start(home, port_override)` starts the same proxy assembly
used by `ironwire serve`. It prepares an empty home, loads existing configuration
and consent, acquires exclusive home ownership, binds loopback, restores quota
and spend, and starts the configured maintenance tasks. It does not repoint tools
or grant subscription, body-capture, or contribution consent.

```rust,no_run
# async fn example() -> Result<(), ironwire_proxy::embed::EmbedError> {
let home = std::path::Path::new("/path/to/.ironwire");
let mut proxy = ironwire_proxy::embed::start(home, None).await?;
let port = proxy.port(); // actual bound port; Some(0) requests an ephemeral port
// A host can select between its own shutdown request and proxy.wait().
// proxy.is_finished() supports hosts that poll their existing lifecycle loop.
proxy.shutdown().await;
# Ok(())
# }
```

Embedded starts use `UpdatePolicy::HostManaged`: the embedding application's
release process upgrades this library. They neither fetch standalone IronWire
release notifications nor load cached installer commands left by a prior CLI
run. The control status reports `update: {"state":"unknown"}`; the host can
present its own application update UI. The CLI explicitly selects
`UpdatePolicy::Standalone` through `start_with_policy`, preserving its existing
release checks and cached notifications. Signed provider-catalog refresh still
honors `updates.check`, and provider model discovery is unchanged for both hosts.

`UpdatePolicy` says who owns upgrading the binary; it does not say whether the
release check and the catalog refresh happen. That remains `updates.check`, whose
only home is `$IRONWIRE_HOME/config.toml` — a file an embedding host may not own,
because the home can be a real user's and shared with the CLI.
`start_with_options` takes the same switch in code:

```rust,no_run
# async fn example() -> Result<(), ironwire_proxy::embed::EmbedError> {
use ironwire_proxy::embed::{EmbedOptions, UpdateChecks, start_with_options};
let home = std::path::Path::new("/path/to/.ironwire");
let proxy = start_with_options(
    home,
    None,
    EmbedOptions::default().with_update_checks(UpdateChecks::Off),
    |_, _| {},
)
.await?;
# proxy.shutdown().await;
# Ok(())
# }
```

`UpdateChecks::Off` suppresses the release check and the catalog refresh, the two
requests IronWire makes that are not the user's own work; someone who declines one
means both. There is deliberately no value that turns them back on over a
configured `updates.check = false`. `UpdateChecks::FromConfig` is the default and
is what every existing entry point does, so a standalone install is unaffected.
`StartupReport::update_checks` reports the decision that was actually applied, so
a host can confirm it declined rather than assume it.

**This is not a general outbound kill switch**, and it is not the startup probe.
Startup catalogue discovery probes registered backends over the network under
either value, and a registered backend is not the same as a configured one:
`build_registry` registers the Claude subscription, Codex subscription, and
Anthropic/OpenAI key backends from credentials found in the environment with no
config entry naming them, and the NEAR AI backend is registered unconditionally so
that `privacy.mode = "full"` has a visible destination. Which backends are probed
is `StartupProbes`, below.

## The startup probe

At startup IronWire calls `Backend::probe` on each registered backend: a live
request that proves the backend works right now and learns the model catalogue the
provider actually serves. It is real work, and a host that declines it is making a
trade, not a saving. Declining means the daemon runs on configured or compiled-in
catalogue values, and an expired credential surfaces on the first real request
rather than at startup. `ironwire doctor` probes on demand and is unaffected.

The reason a host may want to decline is that it did not ask for all of these
requests. A bare embedded start against a home with no `config.toml` still asks
NEAR AI for its model catalogue, because that backend is registered whether or not
anything names it, and a host whose user happens to be logged into Claude Code or
Codex probes those providers too.

```rust,no_run
# async fn example() -> Result<(), ironwire_proxy::embed::EmbedError> {
use ironwire_proxy::embed::{EmbedOptions, StartupProbes, start_with_options};
let home = std::path::Path::new("/path/to/.ironwire");
let proxy = start_with_options(
    home,
    None,
    EmbedOptions::default().with_startup_probes(StartupProbes::Configured),
    |_, _| {},
)
.await?;
# proxy.shutdown().await;
# Ok(())
# }
```

- `StartupProbes::All` is the default and what the CLI does: probe everything
  registered. Nothing changes for a standalone install.
- `StartupProbes::Configured` probes only backends an entry in `config.toml`
  names. A host that declared its backends deliberately keeps the startup answer
  for those, and makes no request on behalf of one IronWire found on its own.
- `StartupProbes::Off` probes nothing. With `UpdateChecks::Off`, this is the only
  combination under which an embedded start makes no outbound request of its own
  accord.

The alternative for a host that wants none of this remains `enabled = false`
entries for each backend, which is again a configuration file it may not own.

Run this inside a Tokio runtime and keep that runtime alive through shutdown.
The application owns the choice to start and stop; no signal handler, tracing
subscriber, or process exit handler is installed by the library. The CLI keeps
its terminal output, actionable port diagnostics, and Ctrl-C/SIGTERM handling.
`startup_report()` gives hosts the startup observations the CLI renders.
`start_with` optionally calls a synchronous announcement hook after successful
binding and assembly, before health can answer or background tasks start. The
CLI uses it to finish its startup instructions before reporting readiness.
The hook must return promptly and cannot wait for the proxy to serve a request.

`wait(&mut self)` observes final completion without giving away shutdown
ownership. It is cancellation-safe and can be read again after completion. It
returns fixed-label `ExitError::Server` or `ExitError::Task` on abnormal exits.
The supervisor waits for the server, cancels and joins housekeeping, flushes
quota, and then releases the pointer and home ownership. Task panics are
containable only with **panic unwinding**; `panic = "abort"` terminates the
process and cannot provide this guarantee. The IronWire CLI's release profile
currently uses abort, so a downstream host must choose its own panic policy.

`shutdown(self)` is graceful: in-flight model streams finish, while the control
event stream closes itself. There is no internal timeout that silently cuts a
model response. Dropping the handle, or canceling an in-progress shutdown
future, requests the same drain in the background; it does **not** release the
home lock early. Await `shutdown` when the host needs proof of cleanup. Dropping
the entire runtime cannot provide graceful completion.

The home contains the same ledger, token, consent, quota, and body files as the
CLI. `daemon.lock.guard` is a persistent lock inode and must not be removed while
an instance can be running. Its OS lock ends when the owner exits; the separate
`daemon.lock` remains a readable actual-port record for older CLIs. Startup also
probes a legacy owner's recorded port. A crashed legacy port record does not
prevent a new start. Older binaries do not participate in the new OS lock, so
this cannot make concurrent startup with an unmodified legacy binary atomic.

The legacy `daemon.lock` probe accepts only a nonzero decimal port in at most
1 KiB of text, including surrounding whitespace. Its health request goes to
numeric IPv4 loopback, ignores environment proxies, and does not follow
redirects. A successful response is advisory grounds to refuse takeover, not
an authenticated claim about the responder.

Legacy port and home-lock opens refuse final-component links/reparse points
and non-regular files. Unix files owned by a different effective user or writable by group or others
are refused based on metadata from the opened handle,
and newly created files use mode 0600. On macOS and Linux x86_64/aarch64, nonblocking opens also
prevent a planted FIFO from blocking before validation; unsupported Unix
platforms refuse this ownership path. This does not confine ancestor paths or
promise a wall-clock bound for arbitrary filesystems. Publication validates the
opened file before truncating it; it is not atomic publication. Cleanup checks
the published inode on Unix and port contents on every platform, keeping the
home lock until the published file handle explicitly closes. Windows cleanup retains the
legacy content check without a file-identity guarantee. A non-cooperating writer
can still replace a path between the cleanup check and deletion. These limits
concern `daemon.lock`; discovery `endpoint.json` has its separate ownership
protocol.

Embedded instances publish `home/endpoint.json` only. The CLI additionally
publishes the conventional `~/.ironwire/endpoint.json` for desktop discovery.
Cleanup checks that a pointer still describes this instance before removing it;
it does not deliberately remove a replacement owner's pointer. That check and
remove are not an atomic compare-and-delete against a non-cooperating writer.
An embedded instance in the conventional home is discoverable there normally;
a custom-home host must arrange any additional discovery explicitly.

Tokens use OS randomness on Unix and Windows. Unix homes and tokens retain modes
0700 and 0600. This extraction does not add Windows ACL hardening; the inherited
implementation relies on the user's profile-directory permissions. No new
package or package version is added: `anyhow`, `reqwest`, and `getrandom 0.2`
become direct uses of packages already in the proxy's dependency tree.

## Pointing a coding tool at an embedded instance

`ironwire_agents::tools::plan_connect` works out the edit; the host shows it and
`commit`s it. For Claude Code the edit fills two slots, and the second one names
a binary:

```rust
use ironwire_agents::tools::{ConnectOptions, StatusLine};
let options = ConnectOptions::default().with_status_line(StatusLine::Decline);
```

`statusLine` is written as `std::env::current_exe()` plus a `statusline`
subcommand. Under the CLI that is `ironwire statusline`, which exists. Under a
host it is the host's own executable, which may not implement the subcommand at
all, or may be a GUI bundle with no command-line surface. Claude Code renders
the command's stdout, so a binary that rejects the argument produces a blank
line rather than a visible failure. A host in that position declines the slot
with `plan_connect_with` and `StatusLine::Decline`; the routing edit is
unchanged, and a status line the user wrote themselves is left alone either way.

Declining also removes a status line IronWire installed on an earlier connect,
so a host that shipped `plan_connect` before adopting this fixes the
installations it already made rather than only the next one. The `installedBy`
marker names IronWire and not a particular binary, so in a home shared with the
`ironwire` CLI this removes the CLI's line as well; `ironwire connect claude`
puts it back.

## Implementation checkpoint

This completes upstream Task 1 of the [Trace Commons private-inference
plan](https://github.com/TraceCommons/trace-commons/pull/609).

- [x] Write lifecycle tests and observe the missing-module compile failure.
- [x] Move startup assembly and its existing helper tests into the library.
- [x] Rewire the CLI to use the same assembly and retain terminal diagnostics.
- [x] Cover ephemeral startup, empty homes, health, owner-only permissions,
      competing/concurrent starts, stale locks, failed-start cleanup, pointer
      ownership, handle drop, graceful stream draining, and observable completion.
- [x] Prove the home-lock test fails when acquisition is removed, then restore it.
- [x] Prepare the verified upstream PR for review.

Warnings-denied `cargo test --all-features --locked --offline` passes with 948 tests and two existing ignored tests. All-target/all-feature Clippy, formatting, and diff checks pass.

The mutation produced `FAILED. 0 passed; 1 failed` at the assertion that the
second start returns `EmbedError::Lock`. Restoring acquisition produced
`ok. 1 passed; 0 failed`. The empty-home test runs in a child process with an
isolated credential environment and blocked external HTTP; other lifecycle tests
disable provider/update discovery and use loopback fixtures only.

**Task 2 has not started.** It must wait for this upstream PR to merge and pin
the merged revision. Its host should use `wait`/`is_finished` for unexpected
completion, keep the runtime alive through draining, respect another instance's
ownership, and explicitly choose a panic policy. The GUI offer remains Task 3,
a separate plan after daemon integration.

Startup diagnostics preserve IronWire's local housekeeping error context. The
host-facing registry refusal carries only a fixed backend construction label,
not the underlying error chain. `StartupReport::home` is the canonical home used
for discovery, including when the caller supplied a symlink. A CLI lock refusal
confirms health before claiming that the recorded port is already running;
an owner still starting or stopping is reported separately.
