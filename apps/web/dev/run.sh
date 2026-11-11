#!/usr/bin/env bash
# Tries the web app on this Mac, end to end, with no AWS, no tailnet and no
# real model: agentd (a debug build) answering from a scripted model
# (fake_llm.py), and devdoor serving apps/web from this folder (edits show on
# reload) the way tailnet-proxy does, with every request signed in as
# you@example.com. The supervisor checks on a task every 6 steps here (30
# minutes or 60 steps on a desktop), so "keep checking" gets its card quickly.
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
(cd services/tailnet-proxy && go build -o "$work/devdoor" ./devdoor)

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
  --no-first-run --no-default-browser-check --host-resolver-rules="MAP form.test 127.0.0.1:8091" \
  about:blank >/dev/null 2>&1 &
pids+=($!)
env -i PATH="$PATH" HOME="$work/home" LISTEN="unix:$sock" STATE_DIR="$work/state" \
  CDP_URL=http://127.0.0.1:9333 RUN_AS= SHELL_TOOL=on POLICY=/nonexistent SKILLS_DIR=/nonexistent \
  EGRESS_USER_HOSTS="$work/state/allowed-hosts.txt" EGRESS_BASE_HOSTS=".github.com pypi.org" \
  MODEL=zai-org/GLM-5.3 LLM_URL=http://127.0.0.1:8091/v1/chat/completions LLM_API_KEY=dev \
  MEMORY_V2=on DESKTOP_NAME=neo-agent RUST_LOG=agentd=info \
  REVIEW_EVERY_STEPS=6 \
  services/agentd/target/debug/agentd >"$work/agentd.log" 2>&1 &
pids+=($!)
for _ in $(seq 50); do
  [ -S "$sock" ] && break
  sleep 0.2
done
"$work/devdoor" -listen 127.0.0.1:8088 -agentd "$sock" \
  -web apps/web/public -novnc http://127.0.0.1:6080 >"$work/devdoor.log" 2>&1 &
pids+=($!)

echo "The web app: http://127.0.0.1:8088/  (logs in $work; Ctrl-C stops it)"
echo "Try: a question; \"sign in\" (Action needed); \"schedule a report\" (a run in two minutes); \"slow\"; \"status site\" (a site to allow);"
echo "     \"write a message\", \"risky command\", \"schedule after reading\" (cards to edit);"
echo "     \"make a plan\" (the plan checklist); \"keep checking\" (the supervisor finds it stuck: it checks every 6 steps here)."
wait
