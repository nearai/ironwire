#!/usr/bin/env bash
# Exercise the packaging scripts against fake release artifacts.
#
# These scripts only ever run inside a tag build, which is the worst possible
# place to discover a typo: the tag is already pushed and the fix needs another
# one. So they run here, on every CI push, against a release-shaped tree.
#
# Run: scripts/test-packaging.sh

set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/ironwire-pkg-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
cd "$root"

pass=0
fail=0
ok()  { printf '  \033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; printf '       %s\n' "${2:-}"; fail=$((fail + 1)); }
skip(){ printf '  \033[2mskip\033[0m %s\n' "$1"; }

VERSION="9.9.9"

# ------------------------------------------------------- fake release artifacts

mkdir -p "${work}/artifacts" "${work}/stage"
printf '#!/bin/sh\necho "ironwire %s"\n' "$VERSION" >"${work}/stage/ironwire"
chmod 755 "${work}/stage/ironwire"
cp "${work}/stage/ironwire" "${work}/stage/ironwire.exe"

for target in aarch64-apple-darwin x86_64-apple-darwin \
              x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
              x86_64-unknown-linux-musl; do
    tar -czf "${work}/artifacts/ironwire-${target}.tar.gz" -C "${work}/stage" ironwire
done
if command -v zip >/dev/null 2>&1; then
    (cd "${work}/stage" && zip -q "${work}/artifacts/ironwire-x86_64-pc-windows-msvc.zip" ironwire.exe)
fi

# ------------------------------------------------------------------ npm

echo "build_npm.mjs"
if out=$(node packaging/build_npm.mjs --version "$VERSION" \
        --artifacts "${work}/artifacts" --out "${work}/npm" 2>&1); then
    ok "runs"
else
    bad "runs" "$out"
fi

shim="${work}/npm/ironwire/package.json"
if [ -f "$shim" ]; then
    name=$(node -e "console.log(require('$shim').name)")
    [ "$name" = "ironwire" ] && ok "publishes an 'ironwire' shim package" \
        || bad "publishes an 'ironwire' shim package" "got $name"

    # A postinstall that downloads is what breaks behind proxies and in
    # offline CI, and it is the classic supply-chain foothold.
    if node -e "process.exit(require('$shim').scripts ? 1 : 0)"; then
        ok "has no install scripts"
    else
        bad "has no install scripts" "package.json declares scripts"
    fi

    count=$(node -e "console.log(Object.keys(require('$shim').optionalDependencies||{}).length)")
    if [ "$count" -ge 4 ]; then
        ok "lists $count per-platform optional dependencies"
    else
        bad "lists per-platform optional dependencies" "only $count"
    fi
else
    bad "publishes an 'ironwire' shim package" "no package.json produced"
fi

# The platform packages must be os/cpu-gated or every user downloads every
# binary — six times the bytes for no benefit.
gated=1
for pkg in "${work}"/npm/cli-*/package.json; do
    [ -e "$pkg" ] || continue
    node -e "const p=require('$pkg'); if(!p.os||!p.cpu) process.exit(1)" || gated=0
done
[ "$gated" = 1 ] && ok "platform packages are os/cpu gated" \
    || bad "platform packages are os/cpu gated" "a package is missing os or cpu"

# The shim must actually run and produce a clear error, not a stack trace.
if [ -f "${work}/npm/ironwire/bin/ironwire.js" ]; then
    if node --check "${work}/npm/ironwire/bin/ironwire.js" 2>/dev/null; then
        ok "the shim is syntactically valid"
    else
        bad "the shim is syntactically valid" "node --check failed"
    fi
    out=$(cd "${work}/npm/ironwire" && node bin/ironwire.js --version 2>&1 || true)
    if grep -qi "no prebuilt binary\|--include=optional" <<<"$out"; then
        ok "a missing platform package produces actionable advice"
    else
        bad "a missing platform package produces actionable advice" "$out"
    fi
fi

# ------------------------------------------------------------------ wheels

echo
echo "build_wheels.py"
if out=$(python3 packaging/build_wheels.py --version "$VERSION" \
        --artifacts "${work}/artifacts" --out "${work}/wheels" 2>&1); then
    ok "runs"
else
    bad "runs" "$out"
fi

count=$(find "${work}/wheels" -name '*.whl' 2>/dev/null | wc -l)
if [ "$count" -ge 4 ]; then
    ok "builds $count platform wheels"
else
    bad "builds platform wheels" "only $count"
fi

wheel=$(find "${work}/wheels" -name '*manylinux*x86_64.whl' | head -n1)
if [ -n "$wheel" ]; then
    if python3 -c "
import zipfile,sys
z = zipfile.ZipFile('$wheel')
bad = z.testzip()
sys.exit(1 if bad else 0)"; then
        ok "the wheel is a valid zip"
    else
        bad "the wheel is a valid zip" "testzip reported corruption"
    fi

    # pip preserves the zip entry's mode; a non-executable binary fails at the
    # least helpful moment, after a successful-looking install.
    if python3 -c "
import zipfile,sys
z = zipfile.ZipFile('$wheel')
entry = next(i for i in z.infolist() if i.filename.endswith('data/scripts/ironwire'))
sys.exit(0 if (entry.external_attr >> 16) & 0o111 else 1)"; then
        ok "the packaged binary is executable"
    else
        bad "the packaged binary is executable" "mode bits lost"
    fi

    if python3 -c "
import zipfile,sys
z = zipfile.ZipFile('$wheel')
names = z.namelist()
sys.exit(0 if any(n.endswith('dist-info/RECORD') for n in names)
              and any(n.endswith('dist-info/METADATA') for n in names)
              and any(n.endswith('dist-info/WHEEL') for n in names) else 1)"; then
        ok "carries RECORD, METADATA and WHEEL"
    else
        bad "carries RECORD, METADATA and WHEEL" "dist-info incomplete"
    fi

    # RECORD hashes are what pip verifies on install.
    if python3 -c "
import base64,csv,hashlib,io,zipfile,sys
z = zipfile.ZipFile('$wheel')
record = next(n for n in z.namelist() if n.endswith('dist-info/RECORD'))
for row in csv.reader(io.StringIO(z.read(record).decode())):
    if not row or not row[1]:
        continue
    data = z.read(row[0])
    want = row[1].split('=', 1)[1]
    got = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b'=').decode()
    if want != got:
        print(f'{row[0]}: {want} != {got}'); sys.exit(1)
"; then
        ok "RECORD hashes match the packaged files"
    else
        bad "RECORD hashes match the packaged files" "pip would reject this wheel"
    fi
fi

# --------------------------------------------------------------- manifest

echo
echo "write_manifest.py"
mkdir -p "${work}/dist"
cp "${work}"/artifacts/*.tar.gz "${work}/dist/"
if out=$(python3 packaging/write_manifest.py --version "$VERSION" \
        --dist "${work}/dist" --out "${work}/dist/manifest.json" 2>&1); then
    ok "runs"
else
    bad "runs" "$out"
fi

m="${work}/dist/manifest.json"
if [ -f "$m" ]; then
    [ "$(jq -r .latest "$m")" = "$VERSION" ] && ok "records the released version" \
        || bad "records the released version" "$(jq -r .latest "$m")"
    [ "$(jq -r '.artifacts | length' "$m")" -ge 4 ] && ok "records artifact checksums" \
        || bad "records artifact checksums" "too few"

    # UPDATES.md / TRUST.md I2: the schema must be structurally unable to
    # redirect an install. A manifest that can name a host is a manifest that
    # can move a user's download somewhere else.
    if jq -e 'tostring | test("https?://|\\bhost\\b|\\burl\\b"; "i")' "$m" >/dev/null 2>&1; then
        bad "cannot express a download location" "manifest contains a URL or host"
    else
        ok "cannot express a download location"
    fi
fi

# ------------------------------------------------------------ yaml/workflow

echo
echo "workflows"
for f in .github/workflows/*.yml; do
    if python3 -c "import sys,yaml; yaml.safe_load(open('$f'))" 2>/dev/null; then
        ok "$(basename "$f") is valid YAML"
    elif ! python3 -c "import yaml" 2>/dev/null; then
        skip "$(basename "$f") (PyYAML not installed)"
    else
        bad "$(basename "$f") is valid YAML" "parse error"
    fi
done

# Every script the release workflow calls must exist. A typo here is only
# discovered by pushing a tag, which is the one thing that cannot be undone.
missing=""
while read -r script; do
    [ -e "$script" ] || missing="$missing $script"
done < <(grep -ohE 'packaging/[A-Za-z0-9_./-]+\.(py|mjs|yaml|sh)' .github/workflows/*.yml | sort -u)
if [ -z "$missing" ]; then
    ok "every packaging script the release job references exists"
else
    bad "every packaging script the release job references exists" "missing:$missing"
fi

echo
if [ "$fail" -eq 0 ]; then
    printf '\033[32m%d passed\033[0m\n' "$pass"
else
    printf '\033[31m%d failed\033[0m, %d passed\n' "$fail" "$pass"
    exit 1
fi
