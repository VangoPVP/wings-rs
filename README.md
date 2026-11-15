# wings

The vangopvp game-server daemon, written in Rust. It runs on each game machine,
runs every server in Docker, and provides the console websocket, file API,
SFTP, backups and transfers. A Panel drives it over HTTP.

## API docs (Swagger UI)

Set `api.docs: true` in `config.yml` (it's on in `dev/config.yml`) and open
**`http://<node>:8080/docs`**. The dropdown has two specs:

| Spec | JSON | What it is |
|---|---|---|
| **wings API** (Panel → daemon) | `/docs/wings.json` | Everything wings serves: `/api/*`, the websocket, downloads and uploads. Click **Authorize** and paste the node `token` to try `/api/*` calls. |
| **Panel remote API** (daemon → Panel) | `/docs/panel-remote.json` | The `/api/remote/*` endpoints that **your Panel must implement**. wings calls these. |

`/docs` doesn't require auth, so leave `api.docs` off (the default) on
production nodes. You can also print the specs without a config or Docker,
for example to generate client code:

```bash
cargo run -- openapi wings  > wings.json
cargo run -- openapi remote > panel-remote.json
```

[API.md](API.md) covers the same protocol in prose. It includes the parts
OpenAPI describes poorly: the websocket events, JWT claims, the transfer
sequence and the egg config parsers.

## Layout

```
src/
  main.rs        CLI entry point: daemon or `openapi`
  router/        HTTP handlers (axum)
  openapi/       OpenAPI specs + Swagger UI (doc-only, handlers untouched)
  protocol/      wire types shared with the Panel (all derive ToSchema)
  remote.rs      client for the Panel's /api/remote/*
  server/ environment/ filesystem/ backup/ sftp/ configs/ ...
dev/             dev config.yml, tiny test image (yolk), runtime data
```

## Local development

Requires Docker (your user in the `docker` group) and Rust 1.99+.

```bash
docker build -t wings-test-yolk dev/yolk      # optional tiny test image
cargo run                                     # API on :8080, SFTP on :2022
```

`cargo run` uses `dev/config.yml` (set in `.cargo/config.toml`). A built
binary defaults to `/etc/pterodactyl/config.yml`; `--config <path>` or an
exported `WINGS_CONFIG` overrides both.

`dev/config.yml` expects a Panel at `remote` (`http://127.0.0.1:3001`, the
vango-panel dev server) with `token_id`/`token` matching it. In vango-panel,
`bun run seed:dev` creates that node. If the Panel isn't reachable, wings still
serves the API (`/docs`, `GET /api/system`) and SFTP. It keeps retrying the
Panel in the background (backoff 1 s to 60 s, logging the real cause, e.g.
`Connection refused`) and loads its servers as soon as the Panel answers.
While it waits, unknown servers return 503 instead of 404, and `states.json`
isn't overwritten.

## Security model

- **File access** goes through `cap-std` sandboxing (`openat2(RESOLVE_BENEATH)`),
  so symlinks and `..` can't escape a server's directory. Archive extraction
  is bomb-safe and never follows links.
- **Containers** run with a read-only rootfs, `no-new-privileges`, dropped
  capabilities, and cgroup limits on memory, CPU, PIDs and IO.
- **Panel API** (`/api/*`) requires the node token, compared in constant time.
- **Downloads and tokens:** remote downloads are SSRF-protected. Browser tokens
  are short-lived JWTs, download links are single-use, and websocket tokens
  can be revoked (`POST /api/servers/{uuid}/ws/deny`).

## Production notes

- Run it as root, with the Panel-generated `config.yml` at `/etc/pterodactyl/config.yml`
  (or pass `--config` / set `WINGS_CONFIG`).
- Use a dedicated `system.user`.
- Enable `api.ssl` (point it at your certificates).
- Keep `api.docs` off.

## Tests

```bash
cargo test
cargo clippy --all-targets
```

`openapi::tests` fails if a route or a `RemoteClient` call is missing from the
specs, or if a schema `$ref` doesn't resolve.
