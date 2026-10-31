# Agent control: design

*Status: draft, 2026-10-30. Facts below were checked against vendor docs on that date (sources at the end). Anything not yet confirmed in practice is marked UNVERIFIED.*

## 1. Goal and requirements

An agent should fully control one cloud desktop (see [PLAN.MD](../PLAN.MD) and the step-1 setup in the [README](../README.md)):

- Run shell commands, install tools, clone repos, change code, commit and push.
- Use web services such as Gmail and Slack in the VM's own logged-in Chrome.
- Work unattended. A person only steps in for logins, 2FA and CAPTCHAs, through the noVNC viewer.

Constraints:
- **No dependency on Claude.** The system must run with a model hosted by another provider (Together AI). Claude is optional and is only used to check the agent's work.
- **No inbound ports, no SSH.** Access is through SSM today and through the control plane later. Desktops can't reach each other.
- **Never store third-party passwords** ([PLAN.MD](../PLAN.MD) ground rule 4). People log in themselves through the viewer, and the session persists in the Chrome profile.
- **The same design must grow into the multi-user product:** the control-plane task API in PLAN M3/M4.

## 2. Recommended: model-agnostic task runner (Together AI worker, optional Claude checker)

```
 you / Claude Code on the Mac
        │  make task PROMPT=...        (phases 1–2: over SSM; phase 3: control plane → :8080)
        ▼
 ┌──────────────── VM (all as user `agent`) ─────────────────────────────────────────┐
 │ cdt-task runner ──► opencode run (worker model on Together AI)                    │
 │      │                 ├─ shell, git, gh, build tools                              │
 │      │                 └─ Playwright MCP ──CDP 127.0.0.1:9222──► logged-in Chrome  │
 │      ├─ collects evidence itself: git diff, test output, screenshot, open tabs     │
 │      └─► checker: Claude API | Together model | none                              │
 │             → pass → done                                                          │
 │             → fail → resume the worker with next_instructions (≤ max_attempts)     │
 │             → needs_user → wait (you log in via `make viewer`, then resume)        │
 └────────────────────────────────────────────────────────────────────────────────────┘
```

### 2.1 Worker framework: opencode

[opencode](https://opencode.ai/docs/cli/) v1.19.5 (2026-10-28) is an open-source coding agent:
- **Install:** official linux-arm64 build, `curl -fsSL https://opencode.ai/install | bash` or `npm install -g opencode-ai`. We install a pinned version with Ansible.
- **Together AI** is a built-in provider. Models are named `togetherai/<model-id>`.
- **One-shot runs:** `opencode run --auto --format json --dir <dir> "<task>"` streams JSON events. Without `--auto`, a permission prompt is refused automatically, so a run never hangs.
- **Sessions:**
  - Resume with `opencode run -s <session-id> ...` or `-c`.
  - List with `opencode session list --format json`.
  - Export a transcript with `opencode export <session-id>`.
- **Server mode:** `opencode serve` gives a REST and SSE API on 127.0.0.1:4096. We don't need it in phases 1–2.
- **Config:** `~/.config/opencode/opencode.json`. Set `"permission": "allow"` for unattended runs; MCP servers go under `"mcp"`.

Alternatives considered:
- **OpenHands:** the CLI README says it is "no longer actively maintained".
- **Goose:** no native Together provider; it would have to go through its OpenAI-compatible mode (UNVERIFIED).

### 2.2 Browser: Playwright MCP attached to the running Chrome

- [Playwright MCP](https://github.com/microsoft/playwright-mcp) v0.0.83 with `--cdp-endpoint http://127.0.0.1:9222`, and without `--isolated`, uses the existing Chrome's default context. That means the same profile, cookies and tabs.
- It works from the accessibility tree, so text-only models can drive it.
- It needs Node ≥18; Ubuntu 26.04 ships Node 22.22. We install it globally with npm at a pinned version rather than running `npx` each time.
- **Alternative:** [Chrome DevTools MCP](https://github.com/ChromeDevTools/chrome-devtools-mcp) v1.12.0 with `--browser-url=http://127.0.0.1:9222 --no-usage-statistics`. Whether it sees tabs that are already open is UNVERIFIED.
- **Sign-in:**
  - Our Chrome launch doesn't set `navigator.webdriver`, because a fixed `--remote-debugging-port` is exempt.
  - Google still blocks sign-in from automated browsers, so **the agent never logs in**. When it reaches a login, 2FA or CAPTCHA page, it stops with `NEEDS_USER`.
- A Chrome policy turns the password manager off (`PasswordManagerEnabled: false`).

opencode config sketch (UNVERIFIED: the keys come from the docs, but the file hasn't been tested):

```json
{
  "$schema": "https://opencode.ai/config.json",
  "model": "togetherai/zai-org/GLM-5.3",
  "permission": "allow",
  "mcp": {
    "browser": {
      "type": "local",
      "enabled": true,
      "command": ["playwright-mcp", "--cdp-endpoint", "http://127.0.0.1:9222"]
    }
  }
}
```

### 2.3 Models (Together AI serverless, prices per 1M tokens)

| Role | Model ID | Input (cached) | Output | Context | Tools | Vision |
|---|---|---|---|---|---|---|
| Worker, default | `zai-org/GLM-5.3` | $1.40 ($0.26) | $4.40 | 1M | yes | no |
| Worker, cheap or browser-heavy | `zai-org/GLM-5.3-Flash` | $0.15 ($0.03) | $0.50 | 1M | yes | no |
| Worker, hard tasks or screenshots; non-Claude checker | `moonshotai/Kimi-K3` | $3.00 ($0.30) | $15.00 | 1M | yes | yes |
| Cheap screenshot reading | `nvidia/NVIDIA-Nemotron-3.5-Super-VL` | $0.15 ($0.02) | $0.50 | 262K | yes | yes |
| Alternative coder | `deepseek-ai/DeepSeek-V4.1-Flash-1017` | $0.30 ($0.006) | $1.20 | 1M | yes | no |

API notes:
- **Endpoint:** OpenAI-compatible at `https://api.together.ai/v1`, with `Authorization: Bearer $TOGETHER_API_KEY`.
- **Tool calling:** `tool_choice` accepts auto, required, none or a named function. Parallel and streamed tool calls work, and `strict: true` is accepted.
- **Structured output** with `json_schema`, and image input (a 560px tile costs 1,601 tokens).

Gotchas:
- **Reasoning field:** GLM and Kimi K3 return a `reasoning_content` field that must be sent back with each request; K3 behaves inconsistently otherwise. Whether opencode does this is UNVERIFIED and is the first thing to test.
- **K3 settings:** K3 defaults to `reasoning_effort=max`, its temperature is fixed at 1.0, and some sampling options return a 400 error.
- **No published rate limits:** expect 429/503 errors and retry with backoff. Guaranteed throughput needs a monthly provisioned-throughput contract.
- **Retired models:** Qwen3-Coder, Kimi K2 and Llama 4 are gone from serverless; MiniMax M3 goes on 2026-11-06. Pin model IDs in config and check the [deprecations page](https://docs.together.ai/docs/deprecations).
- **Coordinate clicking:** no model's ability to click by screen coordinates is documented. Prefer accessibility-tree browsing.
- **Privacy:** by default Together stores prompts and outputs. **Turn on zero data retention** at the organization level before the agent reads email or Slack.

### 2.4 Task runner `cdt-task`

A small Python 3.14 program on the VM. It provides a CLI (`submit`, `status`, `log`, `resume`, `cancel`) and a systemd service that works through queued tasks.

Each task gets its own folder, `~/tasks/<id>/`:

| File | Contents |
|---|---|
| `task.json` | prompt, optional repo URL and check command (e.g. `make test`), worker model, checker, `max_attempts`, time limit |
| `status.json` | `queued`, `running`, `verifying`, `needs_user`, `done`, `failed` or `cancelled`, plus attempt number, opencode session ID and token/cost totals |
| `attempt-N/` | opencode JSON events, exported transcript, `evidence/`, `verdict.json` |

The loop:
1. **"Before" evidence:** a screenshot and the repo's starting commit, if there is a repo.
2. **Run the worker:** `opencode run --format json -s <session> ...` with the task prompt plus a short task contract:
   - work in `~/work/<repo>`;
   - finish with a report covering what you did, how to check it, and what you couldn't do;
   - on a login, 2FA or CAPTCHA, stop and say `NEEDS_USER: <what>`.
3. **Collect evidence independently of the worker's claims:**
   - `git diff <base>` and `git status`, truncated with a visible note when large;
   - the check command's exit code and the end of its output;
   - a final screenshot (scrot);
   - the open tabs and their URLs from `http://127.0.0.1:9222/json/list`;
   - a summary of tool calls from the transcript.
4. **Check:** send the task, the worker's report and the evidence to the checker, which returns:
   ```json
   {"verdict": "pass|fail|needs_user", "issues": [{"severity": "...", "detail": "...", "evidence": "..."}], "next_instructions": "..."}
   ```
5. **Act on the verdict:**
   - `fail` and attempts remain: resume the same opencode session with `next_instructions`;
   - `needs_user`: wait for `cdt-task resume <id>`;
   - `pass`: done.

The checker is configured in `/etc/cloud-desktops/runner.toml`:
- **`claude`:** the Anthropic SDK, model `claude-opus-5-5` ($4 / $20 per 1M input/output tokens, 1M context, image input).
  - The verdict comes back via structured outputs (`output_config.format`), because forcing a tool call isn't supported on this model.
  - The fixed checker instructions are cached.
  - Sonnet 5.5 ($2 / $10) and Haiku 5.5 ($0.10 / $0.50) are cheaper options.
- **`openai_compat`:** a Together model, preferably from a different family than the worker (e.g. Kimi K3 checking GLM), using `response_format: json_schema`.
- **`none`:** the worker's own report is the result.

### 2.5 How tasks get to the VM (replacing Remote Control)

- **Phases 1–2: SSM.** You run the targets below, or ask Claude Code on the Mac to run them. Each one is a command on the VM as `agent`, sent over SSM.
  - `make task PROMPT='...' [REPO=...] [CHECK='make test']`
  - `make task-status ID=...`
  - `make task-log ID=...`
  - `make task-resume ID=... [MSG=...]`
- **Phase 3: control plane.** The same operations become HTTP endpoints on the desktop daemon's port 8080, which only the control plane can reach. The control plane's `POST /desktops/{id}/tasks` (PLAN M4) calls them. The checker moves into the control plane so a compromised worker can't touch its inputs or verdict.

### 2.6 Secrets

- **Storage:** the Together and Anthropic API keys are SSM Parameter Store SecureStrings under `/cloud-desktops/dev/llm/`.
- **Setting them:** `make llm-keys` reads the values without echoing them. They never go into git, Terraform state or logs.
- **Phases 1–2:** Ansible reads them on the Mac with your AWS credentials (`amazon.aws.ssm_parameter` lookup, `no_log: true`) and writes `~agent/.config/cloud-desktops/llm.env` (mode 0600). No IAM change is needed.
- **Phase 3:** each desktop gets its own keys or a proxy, issued by the control plane.

### 2.7 Cost (estimates, to be measured in phase 1)

| Task | Assumptions | Cost |
|---|---|---|
| Coding task on GLM-5.3 | ~40 model calls × ~30k context, ~85% cached input, ~20–60k output (GLM always thinks) | ≈ $0.60–1.00 |
| Browser task on GLM-5.3-Flash | ~30 calls × ~20k context | ≈ $0.03–0.10 |
| Claude Opus 5.5 check | ~25k in (diff, logs, 2 screenshots), ~2–5k out | ≈ $0.15–0.20 per attempt |
| Kimi K3 check | same input | ≈ $0.10–0.15 |

### 2.8 Resources on the t4g.medium (4 GiB plus 2 GiB zram)

- Chrome uses 1–2.5 GB.
- opencode, Playwright MCP and the runner add an estimated few hundred MB (UNVERIFIED; measure in phase 1).
- If memory gets tight, change the instance type variable `desktop_instance_type`. The resize happens in place (stop/start) and keeps the disk.

## 3. Alternative: Claude-native (optional add-on)

Claude Code runs on the VM as `agent`:
- **Shell** is native.
- **Browser:** the [Claude in Chrome](https://code.claude.com/docs/en/chrome) extension. Its native-messaging manifest has to be linked into `--user-data-dir`.
- **Reachability:** [Remote Control](https://code.claude.com/docs/en/remote-control) (`claude remote-control --chrome --permission-mode auto`, run in tmux under systemd). It makes outbound HTTPS calls only.
- **From the Mac:** this session messages it with [cross-session messaging](https://code.claude.com/docs/en/cross-session-messaging) (`ListAgents` / `SendMessage`, Remote Control connected on both sides). You can also use claude.ai/code or the mobile app.

Requirements and limits:
- A **claude.ai login** for both Remote Control and the extension. API keys and `claude setup-token` work for neither.
- **One-time setup** in the viewer: log in, trust the work folder, enable Remote Control, sign in to the extension.
- **Remote Control transcripts** are stored by Anthropic.
- **Permission mode:** auto, where a classifier checks each action, including ones driven by hostile web or email content. Bypass mode is meant only for machines without internet access.

It can run alongside option 2 on the same VM: both attach to the same Chrome, through the extension and through CDP respectively.

## 4. Comparison

| | Model-agnostic runner (recommended) | Claude-native |
|---|---|---|
| Depends on Anthropic | no (Claude checker optional) | yes |
| Auth and billing | API keys, pay per token | claude.ai subscription login on each VM |
| Browser control | Playwright MCP, accessibility tree; quality depends on the worker model | Claude in Chrome extension, mature, pauses for logins |
| How tasks arrive | SSM now, control-plane API later (ours) | Remote Control (Anthropic's) |
| Fit for the multi-user product | good: per-task API, model choice, per-user cost tracking | weak: needs each user's own Claude login |
| Checking | independent evidence plus a configurable checker | the model checks itself (plus auto-mode safety checks) |
| What we build | runner, checker, task API | mostly configuration |
| Data handling | Together (turn on zero data retention), plus Anthropic if checking with Claude | Anthropic (Remote Control transcripts) |

## 5. Security

- **Prompt injection is the main risk.** Email and web content can try to steer the worker. Mitigations:
  - a checker that sees independently collected evidence;
  - per-task time and attempt limits;
  - the agent never logs in;
  - the Chrome password manager is off.
- **Passwordless sudo for `agent` is dev-only** (Ansible variable, off by default). With it, the worker can read anything on the VM, including the LLM keys. Use separate API keys with spending limits for each environment.
- **In the product, the checker runs outside the VM** (control plane), and evidence comes from sources the worker can't modify: the pushed branch on GitHub, and screenshots from the desktop daemon.
- **Unchanged:** no inbound rules, no SSH, desktops isolated from each other. The instance role stays SSM core plus CloudWatch.
- **Data:** turn on Together zero data retention. If checking with Claude, Anthropic's API data retention applies.

## 6. Phased rollout

Each phase ends with something visible working.

### Phase 1: the worker on its own
- **New Ansible role `agent_worker`:**
  - packages: nodejs, npm, git, gh (from GitHub's apt repo), ripgrep, jq, build-essential;
  - opencode, pinned version;
  - Playwright MCP, pinned version via `npm install -g`;
  - `opencode.json` from a template;
  - `llm.env` from SSM;
  - workspace `~/work`;
  - Chrome policy with the password manager off.
- **New `make` targets:** `llm-keys`, `vm-run`, and `task` / `task-log`. For now `task` is a thin wrapper around `opencode run` that saves its output under `~/tasks/<id>/`.
- **GitHub:** a one-time `gh auth login` (device flow), done by you through the viewer.
- **Done when:**
  1. a coding task clones a repo, changes it and pushes a branch;
  2. a browser task works in the logged-in Chrome (e.g. "search X on google.com and report the top 3 results");
  3. checking is by hand: you, or Claude Code on the Mac reviewing the transcript;
  4. memory and cost are measured.

### Phase 2: runner and checker
- The `cdt-task` runner and systemd service, evidence collection, the three checker adapters, and the `needs_user` flow.
- **Done when:** a deliberately flawed result (e.g. a failing test) is caught by the checker and fixed on retry, and a login wall pauses the task until resumed.

### Phase 3: control-plane integration
- The task API on the desktop daemon's port 8080, the control-plane endpoints (PLAN M4), checking in the control plane, and notifications for `needs_user`.

## 7. Open questions

- Together account setup: zero data retention, spending limits, a key for dev.
- Checker default: Claude Opus 5.5, or a cheaper Claude model, or Kimi K3.
- Whether to add the Claude-native option as well.
- To confirm in phase 1 (UNVERIFIED):
  - whether opencode reads `TOGETHER_API_KEY` from the environment (otherwise a `{env:...}` value in the config, or `auth.json`);
  - that it sends `reasoning_content` back correctly;
  - the real memory footprint.

## Sources (checked 2026-10-30)

- **Together AI:**
  [serverless models](https://docs.together.ai/docs/serverless-models),
  [recommended models](https://docs.together.ai/docs/inference/recommended-models),
  [OpenAI compatibility](https://docs.together.ai/docs/inference/openai-compatibility),
  [function calling](https://docs.together.ai/docs/inference/function-calling/best-practices),
  [reasoning](https://docs.together.ai/docs/inference/chat/reasoning),
  [vision](https://docs.together.ai/docs/inference/vision/overview),
  [rate limits](https://docs.together.ai/docs/serverless/rate-limits),
  [zero data retention](https://docs.together.ai/docs/zero-data-retention),
  [deprecations](https://docs.together.ai/docs/deprecations),
  [pricing](https://www.together.ai/pricing)
- **opencode:**
  [CLI](https://opencode.ai/docs/cli/),
  [permissions](https://opencode.ai/docs/permissions/),
  [server](https://opencode.ai/docs/server/),
  [MCP servers](https://opencode.ai/docs/mcp-servers/),
  [providers](https://opencode.ai/docs/providers/),
  [releases](https://github.com/anomalyco/opencode/releases),
  [models.dev](https://models.dev/api.json)
- **Browser:**
  [Playwright MCP](https://github.com/microsoft/playwright-mcp),
  [Chrome DevTools MCP configuration](https://github.com/ChromeDevTools/chrome-devtools-mcp/blob/main/docs/configuration.md),
  [Chrome remote-debugging-port changes](https://developer.chrome.com/blog/remote-debugging-port),
  [Chromium runtime_features.cc](https://chromium.googlesource.com/chromium/src/+/main/content/child/runtime_features.cc),
  [Google: blocking sign-in from automated browsers](https://security.googleblog.com/2019/04/better-protection-against-man-in-middle.html)
- **Alternatives:**
  [OpenHands headless](https://docs.openhands.dev/openhands/usage/cli/headless),
  [Goose releases](https://github.com/aaif-goose/goose/releases)
- **Claude Code:**
  [setup](https://code.claude.com/docs/en/setup),
  [authentication](https://code.claude.com/docs/en/authentication),
  [Remote Control](https://code.claude.com/docs/en/remote-control),
  [Chrome](https://code.claude.com/docs/en/chrome),
  [cross-session messaging](https://code.claude.com/docs/en/cross-session-messaging),
  [permission modes](https://code.claude.com/docs/en/permission-modes)
- **Claude models and pricing:** Anthropic API model table (cached 2026-10-06), via the claude-api reference
