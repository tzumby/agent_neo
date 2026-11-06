#!/usr/bin/env bash
# Tries the web app on this Mac, end to end, with no AWS and no real model:
# agentd (a debug build) answering from a scripted model (fake_llm.py),
# front-door serving apps/web from this folder (edits show on reload), and
# devedge in front of it, signing you in the way Cloudflare Access would.
#
#   make web-dev      then open http://127.0.0.1:8088/
#
# The desktop view needs a VNC server, so it stays blank here. State is
# thrown away when this stops; WORK=<dir> keeps it.
set -euo pipefail

cd "$(dirname "$0")/../../.."
work=${WORK:-$(mktemp -d "${TMPDIR:-/tmp}/agent-web.XXXXXX")}
mkdir -p "$work/state" "$work/home"
# A Unix socket's path has to be short (104 bytes on macOS).
sockets=$(mktemp -d /tmp/agent-web.XXXXXX)
sock=$sockets/agentd.sock

(cd services/agentd && cargo build --quiet)
(cd services/front-door && go build -o "$work/front-door" . && go build -o "$work/devedge" ./devedge)

pids=()
cleanup() {
  kill "${pids[@]}" 2>/dev/null || true
  rm -rf "$sockets"
  [ -n "${WORK:-}" ] || rm -rf "$work"
}
trap cleanup EXIT INT TERM

python3 apps/web/dev/fake_llm.py 8091 &
pids+=($!)
# agentd drives a browser: a headless Chrome of its own, as its evals do.
chrome=${CHROME:-"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"}
"$chrome" --headless=new --remote-debugging-port=9333 --user-data-dir="$work/chrome" \
  --no-first-run --no-default-browser-check about:blank >/dev/null 2>&1 &
pids+=($!)
env -i PATH="$PATH" HOME="$work/home" LISTEN="unix:$sock" STATE_DIR="$work/state" \
  CDP_URL=http://127.0.0.1:9333 RUN_AS= SHELL_TOOL=on POLICY=/nonexistent SKILLS_DIR=/nonexistent \
  EGRESS_USER_HOSTS="$work/state/allowed-hosts.txt" EGRESS_BASE_HOSTS=".github.com pypi.org api.cloudflare.com .datadoghq.com" \
  MODEL=zai-org/GLM-5.3 LLM_URL=http://127.0.0.1:8091/v1/chat/completions LLM_API_KEY=dev \
  MEMORY_V2=on DESKTOP_NAME=acme-agent RUST_LOG=agentd=info SLACK_API_URL=http://127.0.0.1:8091/api \
  services/agentd/target/debug/agentd >"$work/agentd.log" 2>&1 &
pids+=($!)
"$work/devedge" -write-config "$work/access.json" >"$work/devedge.log" 2>&1 &
pids+=($!)
for _ in $(seq 50); do
  [ -S "$sock" ] && [ -f "$work/access.json" ] && break
  sleep 0.2
done
"$work/front-door" -config "$work/access.json" -listen 127.0.0.1:8090 -agentd "$sock" \
  -web apps/web/public -novnc http://127.0.0.1:6080 >"$work/front-door.log" 2>&1 &
pids+=($!)

echo "The web app: http://127.0.0.1:8088/  (logs in $work; Ctrl-C stops it)"
echo "Try: a question; \"sign in\" (Action needed); \"schedule a report\" (a run in two minutes); \"slow\"; \"status site\" (a site to allow);"
echo "     after adding Slack under Updates (any xoxb-… token), \"post an update\". Slack's stand-in: http://127.0.0.1:8091/slack-posts"
wait
