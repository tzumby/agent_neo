---
name: fly-deploy
description: Deploying to Fly.io with flyctl from this desktop, where the build has to work through the proxy.
---
# Deploying to Fly.io

A plain `fly deploy` fails here. Its default builder (Depot) connects to a
build machine by IP address, outside the proxy, and the shell has no DNS of its
own. Use these steps instead of working that out again.

## Set up

1. Install it with Nix: `nix profile add nixpkgs#flyctl`, then `fly version`.
   Token mode (below) is known to be in 0.4.123 and later.
2. The token: FLY_API_TOKEN among your saved keys. If it isn't there, call
   request_secret:
   - name `FLY_API_TOKEN`, label `Fly.io token`
   - how:
     1. On the Fly.io dashboard, open the app and click Tokens, or on your own
        computer run `fly tokens create deploy -a <app>`.
     2. Copy the whole value, including the `FlyV1` at the start.
3. Call request_access once for `api.fly.io`, `api.machines.dev` and
   `gateway.machines.dev`.

## Deploy

Run this with `secrets: ["FLY_API_TOKEN"]` and `timeout: 900`, since builds are
slow:

    FLY_WIREGUARD_TOKEN_MODE=1 fly deploy --depot=false --builder-pool=false --remote-only

- `--depot=false --builder-pool=false`: the organization's own builder. Both of
  the others are reached by direct connections.
- `FLY_WIREGUARD_TOKEN_MODE=1`: the tunnel to that builder runs over a WebSocket
  to `gateway.machines.dev`, by name, through the proxy.
- `--remote-only`: there's no Docker on this desktop.

`fly ssh console` uses the same tunnel, so give it the same variable and the
token: `FLY_WIREGUARD_TOKEN_MODE=1 fly ssh console -a <app> -C '<command>'`.
`fly status`, `fly logs` and `fly secrets` only need the proxy.

## These don't work here

- `fly deploy` as it comes, or `--depot=true`: Depot's build machines are IP
  addresses, so request_access can't allow them. `api.depot.dev` alone isn't
  enough.
- `--wg=false`: it connects to the builder directly.
- WireGuard without token mode: it looks up the gateway's name first, and
  there's no DNS.
- `--local-only`: there's no Docker.

## If the deploy still fails

Token mode is new, and this route hasn't been proven on every setup. If the
token is refused, ask for a new one with request_secret (`replace`). An
organization token (`fly tokens create org`) may work where an app's doesn't.

Otherwise stop after one failed attempt. Tell the user the exact error, and
offer the two ways that don't depend on it:

1. **This one deploy with `direct_internet: true`**: plain `fly deploy`, with
   the token, and no flags or hosts needed. The user approves it on a card.
2. **A GitHub Actions workflow in the app's repository**, for deploys from now
   on. Actions runners connect freely. The user adds the token to the
   repository's secrets, and you watch the run with `gh run watch`.

A build over 15 minutes doesn't fit in one command here, so use Actions for it.
