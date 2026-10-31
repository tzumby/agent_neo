---
name: node
description: Node.js projects: installing dependencies, running scripts and one-off CLIs.
---
# Node.js

node and npm come from Nix (`node --version`). Another version for one
command: `nix shell nixpkgs#nodejs_22 -c node --version`.

- Dependencies: `npm install` in the project. With `pnpm-lock.yaml`, use
  `npx pnpm install`; with `yarn.lock`, `npx yarn install`.
- Scripts: `npm run build`, `npm test`.
- One-off CLIs: `npx <tool>`.
- Global CLIs: `npm install -g <package>` installs into ~/.npm-global, which
  is on the PATH.

A command's processes end when the command ends, so a dev server can't keep
running between commands. To check one, start it, test it and stop it in the
same command.
