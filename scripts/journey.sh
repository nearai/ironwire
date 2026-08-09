#!/usr/bin/env bash
# Walk the whole user journey against a mock provider.
#
# Not a unit test and not a substitute for one. This runs the *commands a person
# runs*, in the order they run them, and checks the output says something useful
# at each step. It exists because every integration bug found so far in this
# project was found by doing exactly this and none of them were visible from
# inside a unit test.
#
# Run: scripts/journey.sh   (builds in debug; ~1 minute)

set -uo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/ironwire-journey.XXXXXX")
bin="${root}/target/debug/ironwire"
port=18999
mock_port=19000
daemon_pid=""
mock_pid=""

cleanup() {
    [ -n "$daemon_pid" ] && kill "$daemon_pid" 2>/dev/null
    [ -n "$mock_pid" ] && kill "$mock_pid" 2>/dev/null
    wait 2>/dev/null
    rm -rf "$work"
}
trap cleanup EXIT INT TERM

export IRONWIRE_HOME="${work}/home"
export CODEX_HOME="${work}/codex"
export ANTHROPIC_API_KEY="sk-ant-journey-test"
export IRONWIRE_ANTHROPIC_BASE_URL="http://127.0.0.1:${mock_port}"
mkdir -p "$IRONWIRE_HOME" "$CODEX_HOME"

pass=0
fail=0
ok()   { printf '  \033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$1"; printf '       %s\n' "${2:-}" | head -20; fail=$((fail + 1)); }
step() { printf '\n\033[1m%s\033[0m\n' "$1"; }

# Assert that running <cmd> succeeds and its output contains <needle>.
says() {
    local label="$1" needle="$2"; shift 2
    local out
    if ! out=$("$@" 2>&1); then
        bad "$label" "command failed: $*
$out"
        return
    fi
    if grep -qF -- "$needle" <<<"$out"; then
        ok "$label"
    else
        bad "$label" "expected to see '$needle' in:
$out"
    fi
}

# ------------------------------------------------------------------ mock

start_mock() {
    python3 - "$mock_port" <<'PY' &
import http.server, json, socketserver, sys, threading

SSE = (
    b'event: message_start\n'
    b'data: {"type":"message_start","message":{"id":"msg_j","model":"claude-opus-4-6",'
    b'"usage":{"input_tokens":42,"cache_read_input_tokens":900,"output_tokens":1}}}\n\n'
    b'event: content_block_delta\n'
    b'data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}\n\n'
    b'event: message_delta\n'
    b'data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}\n\n'
    b'event: message_stop\ndata: {"type":"message_stop"}\n\n'
)

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "claude-opus-4-6"}]}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.send_header("anthropic-ratelimit-unified-limit", "1000")
        self.send_header("anthropic-ratelimit-unified-remaining", "620")
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        self.rfile.read(length)
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(SSE)))
        self.send_header("anthropic-ratelimit-unified-limit", "1000")
        self.send_header("anthropic-ratelimit-unified-remaining", "620")
        self.end_headers()
        self.wfile.write(SSE)

class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

Server(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
PY
    mock_pid=$!
}

wait_for() {
    local url="$1" tries=50
    while [ $tries -gt 0 ]; do
        curl -fsS -m 1 "$url" >/dev/null 2>&1 && return 0
        sleep 0.2
        tries=$((tries - 1))
    done
    return 1
}

# ------------------------------------------------------------------ journey

[ -x "$bin" ] || { echo "build first: cargo build"; exit 1; }

step "1. A new user runs init"
says "init names capacity it found" "ANTHROPIC_API_KEY" "$bin" init
says "init says what to run next" "ironwire serve" "$bin" init
says "init writes a config on request" "Wrote" "$bin" init --write
says "init does not clobber an existing config" "leaving it alone" "$bin" init --write

step "2. They start the daemon"
start_mock
wait_for "http://127.0.0.1:${mock_port}/v1/models" || { echo "mock did not start"; exit 1; }
"$bin" serve --port "$port" >"${work}/serve.log" 2>&1 &
daemon_pid=$!
if wait_for "http://127.0.0.1:${port}/_ironwire/health"; then
    ok "the daemon comes up and answers health"
else
    bad "the daemon comes up and answers health" "$(cat "${work}/serve.log")"
    exit 1
fi
grep -q "Claude Code: export ANTHROPIC_BASE_URL" "${work}/serve.log" \
    && ok "startup tells you how to point a client at it" \
    || bad "startup tells you how to point a client at it" "$(cat "${work}/serve.log")"

step "3. doctor, before anything is pointed here"
says "doctor notices no client is pointed here" "not pointed here" "$bin" doctor --port "$port"
says "doctor gives the fix" 'eval "$(ironwire env)"' "$bin" doctor --port "$port"

step "4. They point a client at it"
export ANTHROPIC_BASE_URL="http://127.0.0.1:${port}/anthropic"
says "doctor now sees the client" "claude code   pointed here" "$bin" doctor --port "$port"
says "doctor probes the backend" "anthropic-key" "$bin" doctor --port "$port"

step "5. Real traffic"
for i in 1 2 3; do
    curl -fsS -m 10 -X POST "http://127.0.0.1:${port}/anthropic/v1/messages" \
        -H 'content-type: application/json' \
        -d '{"model":"claude-opus-4-6","stream":true,"system":"You are Claude Code","messages":[{"role":"user","content":"turn '"$i"'"}]}' \
        >"${work}/response-${i}.sse" 2>&1
done
grep -q "message_stop" "${work}/response-3.sse" \
    && ok "a request round-trips through the proxy" \
    || bad "a request round-trips through the proxy" "$(cat "${work}/response-3.sse")"

diff <(printf 'event: message_start\n') <(head -1 "${work}/response-1.sse") >/dev/null \
    && ok "the response reaches the client in the provider's own framing" \
    || bad "the response reaches the client in the provider's own framing" "$(head -3 "${work}/response-1.sse")"

step "6. status and log tell them what happened"
says "status shows the backend"          "Anthropic API"   "$bin" status --port "$port"
says "status reports observed capacity"  "% used"          "$bin" status --port "$port"
says "status counts the conversation"    "conversation(s)" "$bin" status --port "$port"
says "status aggregates a balance"       "Balance:"        "$bin" status --port "$port"
says "status measures the session from our own ledger" \
    "measured from IronWire's own ledger" "$bin" status --port "$port"
says "status says how fast the window is going" "tokens/min" "$bin" status --port "$port"
# No completed windows yet and no declared plan, so there is nothing to be a
# percentage *of* — and saying so beats inventing a ceiling to fill the bar.
says "status claims no ceiling it was never given" \
    "nothing to compare against yet" "$bin" status --port "$port"

# Colour has to survive being piped: `says` already captures through a pipe, so
# any escape sequence reaching here would also reach a user's grep or log file.
out=$("$bin" status --port "$port" 2>&1)
if grep -q $'\033' <<<"$out"; then
    bad "piped output carries no escape sequences" "$out"
else
    ok "piped output carries no escape sequences"
fi
out=$("$bin" status --color always --port "$port" 2>&1)
if grep -q $'\033' <<<"$out"; then
    ok "--color always overrides the pipe"
else
    bad "--color always overrides the pipe" "$out"
fi

says "log lists the exchanges"           "claude-opus-4-6" "$bin" log --port "$port"
says "log reports what it cost"          "last 24h"        "$bin" log --port "$port"
says "log shows the tokens the provider reported" "cached" "$bin" log --port "$port"

step "7. The privacy filter, off by default"
says "privacy is off unless asked for" "off" "$bin" privacy status
# The generated config already has a `[privacy]` section, so this edits it
# rather than appending a second one — which TOML rejects, and which is
# exactly the mistake a user following "just add [privacy]" advice would make.
python3 - "${IRONWIRE_HOME}/config.toml" <<'EDIT'
import re, sys
path = sys.argv[1]
text = open(path).read()
text = text.replace('mode = "off"', 'mode = "credentials"')
text = text.replace('named_values = []', 'named_values = ["Acme Holdings"]')
open(path, "w").write(text)
EDIT
says "privacy status reflects the config" "1 named value" "$bin" privacy status
printf 'notes for Acme Holdings\ntoken = "ghp_abcdefghijklmnopqrstuvwxyz0123456789"\n' >"${work}/sample.txt"
says "privacy check finds both tiers"  "2 match" "$bin" privacy check "${work}/sample.txt"
says "privacy check does not reprint the secret" "…" "$bin" privacy check "${work}/sample.txt"

step "8. Shell integration"
says "env emits POSIX by default"   "export ANTHROPIC_BASE_URL" env SHELL=/bin/bash "$bin" env --port "$port"
says "env emits fish when asked"    "set -gx"                   "$bin" env --shell fish --port "$port"
says "completions generate"         "_ironwire"                 "$bin" completions bash

step "9. Service management"
says "service status names the mechanism" "Service manager" "$bin" service status

step "10. Shutting down"
kill "$daemon_pid" 2>/dev/null
wait "$daemon_pid" 2>/dev/null
daemon_pid=""
out=$("$bin" status --port "$port" 2>&1)
if grep -q "not running" <<<"$out"; then
    ok "a stopped daemon says so, rather than a transport error"
else
    bad "a stopped daemon says so, rather than a transport error" "$out"
fi

printf '\n'
if [ "$fail" -eq 0 ]; then
    printf '\033[32m%d passed\033[0m\n' "$pass"
else
    printf '\033[31m%d failed\033[0m, %d passed\n' "$fail" "$pass"
    exit 1
fi
