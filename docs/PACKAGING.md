# Packaging and distribution

Target install commands:

```bash
brew install nearai/tap/ironwire
npx ironwire@latest connect claude
sudo apt install ironwire
pip install ironwire
curl -fsSL https://ironwire.dev/install.sh | sh
```

All five deliver **the same prebuilt Rust binary**. Nothing is built on the
user's machine and nothing is reimplemented per ecosystem — the npm/pip packages
are thin shims that select and exec the right platform binary.

---

## Build matrix

| Target | Notes |
|---|---|
| `aarch64-apple-darwin` | primary |
| `x86_64-apple-darwin` | |
| `x86_64-unknown-linux-gnu` | |
| `aarch64-unknown-linux-gnu` | |
| `x86_64-unknown-linux-musl` | static; what the shell installer prefers on unknown distros |
| `x86_64-pc-windows-msvc` | |

Binary size is worth keeping down — five ecosystems ship this one file and
users re-download it on every update — which is part of why the ironclaw reuse
is feature-gated (DESIGN §7): the default build does not pull `rig-core` or the
AWS SDK.

It is **not a hard budget**. CI reports the size on every build so a jump shows
up in review, and nothing fails on a threshold. A capability worth its bytes
should not be blocked by a number picked in advance; the point is that growth is
a decision someone makes rather than something that happens quietly.

Current default build: **~7 MB stripped** on linux-x64. Most of the growth from
the initial ~5 MB is bundled SQLite for the trace ledger — a deliberate trade:
vendoring it keeps the install a single self-contained file on every platform,
which matters more here than two megabytes.

---

## Mechanism

`cargo-dist` produces the shell installer, the Homebrew formula, the npm
package and the MSI from one config. IronClaw already uses
`[package.metadata.dist]`, so the tooling is familiar in this org.

### Homebrew

`cargo-dist` generates a formula pushed to `nearai/homebrew-tap`. It ships the
binary plus a `brew services` plist so `ironwire serve` can run as a launchd
agent.

### npm

`ironwire` is a thin package whose `postinstall` (or lazy `bin` shim) picks the
matching `@ironwire/cli-<platform>-<arch>` optional dependency and execs its
binary. `npx ironwire@latest` therefore works with no build step. Optional
dependencies with `os`/`cpu` fields mean a user only downloads their own
platform.

### apt

`nfpm` (driven from the same release job) builds a `.deb` carrying the binary, a
systemd **user** unit (`ironwire.service`, `WantedBy=default.target`) and the
shell completions. Hosted at `apt.ironwire.dev` with a signed `Release` file.
Lowest priority of the five: Linux developers reach for the shell installer, and
the apt repo is the only option here that needs ongoing infrastructure.

### pip

A wheel per platform tag (`macosx_11_0_arm64`, `manylinux_2_28_x86_64`, …)
containing the binary and a console-script entry point that execs it. No
`maturin`, no compilation — the wheel is a delivery vehicle. Weakest fit of the
five, but it is the natural channel for Aider users, who install their agent
with pip.

### Shell installer

Detects platform, downloads the matching archive, verifies a checksum, installs
to `~/.ironwire/bin`, and prints the `PATH` line. This is the one to get right
first: it is the fallback for every platform the others miss.

---

## Runtime layout

```
$IRONWIRE_HOME              default ~/.ironwire   (0700)
├── config.toml             user configuration
├── consent.json            recorded subscription consents (TRUST §2)
├── control.token           control-API token (0600)
├── daemon.lock             single-daemon lockfile
├── ledger.sqlite           local trace ledger
└── bodies/                 captured bodies, only if capture.bodies = true
```

Nothing here is world-readable. Nothing here is synced.

## Service management

| Platform | Mechanism |
|---|---|
| macOS | launchd agent (`brew services start ironwire`, or `ironwire service install`) |
| Linux | systemd **user** unit — the daemon holds user credentials and must not run as root |
| Windows | scheduled task at logon |

`ironwire serve` in the foreground stays first-class: it is what the shell
installer tells you to run, and it is what `doctor` assumes.

---

## Release process

1. Tag `vX.Y.Z`.
2. CI builds the matrix, runs the conformance harness (PROTOCOL §7) on
   macos-arm64 and linux-x64, and reports the binary size.
3. `cargo-dist` publishes the GitHub release, the tap commit and the npm
   packages; `nfpm` pushes the `.deb`; the wheel job pushes to PyPI.
4. `install.sh` and `ironwire update` both read the release manifest, so there
   is one source of truth for "what is latest".
