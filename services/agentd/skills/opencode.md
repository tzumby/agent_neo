---
name: opencode
description: Setting up and running opencode, a coding agent CLI, with a Together AI API key the user saves.
---
# opencode with Together AI

1. Install it with Nix: `nix profile add nixpkgs#opencode`, then
   `opencode --version`.
2. The key: TOGETHER_API_KEY among your saved keys. If it isn't there, call
   request_secret:
   - name `TOGETHER_API_KEY`, label `Together AI API key`
   - url `https://api.together.ai/settings/projects/~current/api-keys`
   - how:
     1. Open the link and sign in to Together AI.
     2. Tap **Create key**, name it (e.g. "Cloud desktop") and copy it.
        Together shows it only once.
     3. Come back here and paste it.
3. Configure it without the key, in `~/.config/opencode/opencode.json`:
   ```json
   {
     "$schema": "https://opencode.ai/config.json",
     "model": "togetherai/zai-org/GLM-5.3",
     "autoupdate": false
   }
   ```
   opencode's Together provider (`togetherai`) reads TOGETHER_API_KEY from
   the environment, so no file holds the key. Don't use `/connect` or
   `opencode auth login`: they write the key into
   `~/.local/share/opencode/auth.json`.
4. Run it with run_shell's secrets `["TOGETHER_API_KEY"]`:
   - Models: `opencode models togetherai`
   - A job in a repository:
     `opencode run --dir ~/work/api "Fix the failing test in src/parser.rs"`
     `--auto` lets it run its own commands and edit files without asking.
     They run inside your command, without the user's approvals, so use it
     only for work inside a repository, and give opencode only
     TOGETHER_API_KEY: commit and push yourself afterwards.
   - Use a long timeout (600) for real jobs.
5. A 401 or "invalid API key" means the key is wrong or was revoked: ask
   again with request_secret and `replace` set.

opencode talks to Together AI at api.together.xyz, which the proxy allows.
If it refuses that host anyway, ask with request_access.
