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

A command's processes end when the command ends, and the firewall refuses
every connection to this machine (localhost, 127.0.0.1, ::1) except the
proxy, even to a server the same command started. So you can't run a dev
server and request pages from it here. Check the app with
`npm run build` and tests that don't open a network port (unit tests,
renderers, `next build`). For anything that needs a running server, tell the
user what to run on their own computer.
