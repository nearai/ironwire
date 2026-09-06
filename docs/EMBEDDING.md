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
