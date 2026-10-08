# pghacker-mcp

A blank, local browser application layered on [PilotSwarm](https://github.com/microsoft/PilotSwarm) **0.8.2**. It uses the shipped portal and one app-owned SDK worker, with only an app branding plugin. No pghacker-specific agents, PostgreSQL tools, MCP integrations, or demo sessions are added.

## Run locally

Requires **Node.js 24+**, npm, and a running local Docker engine with Docker Compose. The helper scripts target macOS/Linux and use standard `tar` and `ps` commands. Setup needs access to GitHub Releases and the npm registry.

```bash
npm run setup
npm start
```

Open **http://127.0.0.1:3001**.

Setup downloads the three matching PilotSwarm release tarballs, verifies their pinned SHA-256 checksums, preserves the release license, and installs dependencies. Subsequent setup uses `npm ci` with the existing lockfile. Do not install name-only `pilotswarm` packages from the npm registry.

Setup also creates an ignored `.env` with a random database password and owner-only permissions. Existing `.env` files are preserved and validated. `.env.example` documents the settings; its placeholder password is intentionally unusable.

Startup launches a dedicated PostgreSQL 17 container and waits for its health check, then starts the SDK worker and prebuilt portal with `plugin/plugin.json`. The database is PilotSwarm runtime infrastructure, **not** the deferred PostgreSQL/MCP product integration.

The worker uses PilotSwarm's supported `disableManagementAgents: true` option so the initial workspace stays empty and startup does not run management-agent model calls. `WORKERS=0` prevents the portal from adding its own workers; the launcher owns the single SDK worker and stops it with the portal.

| Service | Address |
| --- | --- |
| Browser portal | `127.0.0.1:3001` |
| Runtime PostgreSQL | `127.0.0.1:55433` |

Both services are restricted to loopback. Sign-in is disabled and local visitors have admin access: **do not expose this starter through port forwarding, a reverse proxy, or a public/LAN listener**. Production authentication and deployment are out of scope.

Compose projects and data volumes are namespaced by the worktree path. Startup fails on port conflicts rather than stopping unrelated services. To select different local ports, edit `PORTAL_PORT`, or both `POSTGRES_PORT` and the matching port in `DATABASE_URL`, in `.env`.

## Model access

The blank shell can start without making a model request. To run agents, configure a usable provider through **Admin Console -> Model Providers**, including provider credentials and defaults.

For optional GitHub Copilot bootstrap, supply `GITHUB_TOKEN` or `GH_TOKEN` in your shell. A GitHub account with the appropriate Copilot access is required; a token that can read public repositories is not proof of model access.

```bash
# With an authenticated GitHub CLI and Copilot access:
GITHUB_TOKEN="$(gh auth token)" npm start
```

An optional `GITHUB_TOKEN` in the ignored `.env` takes precedence over shell tokens. Never commit credentials. The launcher does not import ambient remote database, Azure storage, plugin, worker-count, or authentication settings.

## Stop and restart

Press **Ctrl+C** in the starting terminal to stop the portal. To stop both the portal and its dedicated PostgreSQL container, including a portal running in the background:

```bash
npm run stop
```

The stop command verifies the recorded process identity before signalling its specific PID. It never kills a process just because it occupies a port. PostgreSQL's data volume is preserved; `npm start` restarts the app against the same data.

Keep the generated `.env`: its password must continue to match the existing database volume. Changing database users/passwords in `.env` does not migrate an initialized PostgreSQL database. No destructive reset command is provided.

## Checks

```bash
npm test
npm run smoke  # requires the app to be running
```

Tests cover release integrity, configuration preservation, local isolation, branding, readiness, and safe process handling. The smoke check requires a genuinely started local runtime, one ready app-owned worker in the registry, no extra portal-managed workers, disabled sign-in, and `pghacker-mcp` branding. It does not create sessions or send paid model requests.

The portal is prebuilt in the release package; this repository does not need a separate frontend build. Downloaded releases, `.env`, dependencies, and app-local state under `.runtime/` are ignored by Git.
