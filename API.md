# wings API reference

Contract between the daemon and the vangopvp Panel. Shapes follow Pterodactyl
Wings closely, with JSON bodies on every response and a few additions (marked
**ext**).

The same API as OpenAPI 3.1, with request and response schemas: Swagger UI at
`/docs` (enable `api.docs`), or `wings openapi wings` / `wings openapi remote`.
If this file and the specs disagree, the specs (generated from the Rust types)
are right.

All errors are `{"error": "<message>"}` with an appropriate status code.

## Authentication

| Caller | How |
|---|---|
| Panel → daemon (`/api/*`) | `Authorization: Bearer <node token>` (`token` in `config.yml`) |
| Browser → daemon (websocket, downloads, uploads) | HS256 JWT signed with the **node token**, minted by the Panel |
| Daemon → Panel (`/api/remote/*`) | `Authorization: Bearer <token_id>.<token>` |
| Node → node (transfers) | HS256 JWT signed with the **destination** node's token |

### JWT claims

Common: `exp` (required), `nbf`, `iat`, `jti`. Omit claims you don't use (don't send `null`).

| Token | Extra claims |
|---|---|
| websocket | `user_uuid`, `server_uuid`, `permissions: [..]` |
| file download | `server_uuid`, `file_path`, `unique_id` (single use) |
| backup download | `server_uuid`, `backup_uuid`, `unique_id` (single use) |
| upload | `server_uuid`, `user_uuid`, `unique_id` (reusable until `exp`) |
| transfer | `sub` = server uuid, `jti` (single use) |

Websocket permissions: `websocket.connect` (required), `control.console`,
`control.start`, `control.stop`, `control.restart`, `control.kill`
(`set state kill`), `admin.websocket.errors`, `admin.websocket.install`,
`admin.websocket.transfer`, `backup.read`, or `*` for everything.

## Panel → daemon

### System
| | |
|---|---|
| `GET /api/system` | `{version, architecture, os, kernel_version, os_name, cpu_count, memory_bytes, docker_version, servers, uptime_seconds}` |

### Servers
| Method & path | Body | Response |
|---|---|---|
| `GET /api/servers` | | `[ServerDetails]` |
| `POST /api/servers` | `{uuid, start_on_completion?}` | 202. Daemon fetches config from Panel, installs, optionally starts |
| `GET /api/servers/{uuid}` | | `ServerDetails` |
| `DELETE /api/servers/{uuid}` | | 202. Kills, removes container and files |
| `GET /api/servers/{uuid}/logs?size=100` | | `{"data": ["line", ...]}` |
| `POST /api/servers/{uuid}/power` | `{action: start\|stop\|restart\|kill, wait_seconds?}` | 202 `{status, server_id, action}` (200 for `kill`) |
| `POST /api/servers/{uuid}/commands` | `{commands: ["say hi"]}` | `{status, sent}` (409/400 if not running) |
| `POST /api/servers/{uuid}/install` | | 202 |
| `POST /api/servers/{uuid}/reinstall` | | 202 |
| `POST /api/servers/{uuid}/sync` | | Re-fetches config. Kills the server if it became suspended |
| `POST /api/servers/{uuid}/ws/deny` | `{jtis: [..]}` | Revokes websocket tokens (open sockets get `jwt error`) |

`ServerDetails`:
```json
{"uuid": "...", "state": "offline|starting|running|stopping", "is_suspended": false,
 "is_installing": false, "is_transferring": false, "is_restoring": false,
 "utilization": {"memory_bytes": 0, "memory_limit_bytes": 0, "cpu_absolute": 0.0,
                 "network": {"rx_bytes": 0, "tx_bytes": 0}, "uptime": 0,
                 "state": "offline", "disk_bytes": 0}}
```
`cpu_absolute` is percent of one core (200 = two cores). `uptime` is in ms.

Power semantics: `start` always re-syncs config from the Panel, rewrites egg
config files and **recreates the container** (so limit changes apply).
`stop` sends the egg stop (command/signal/docker stop) and waits up to 10 min
before killing. `restart` = stop + start. `kill` force-removes the server
container (and any leftover `<uuid>_installer` / `<uuid>_chown` container),
cancels any in-flight start/restart/install, and returns once it is gone — it
works whether the process is running, starting, installing or already exited.
`wait_seconds` = how long to wait for another in-flight power action (default: fail immediately with 409).

### Files (`/api/servers/{uuid}/files/...`)
Paths are relative to the server root. `/` and `..` can't escape it. Egg
`file_denylist` patterns are enforced (403).

| Method & path | Body / query | Response |
|---|---|---|
| `GET list-directory?directory=/` | | `[FileStat]` (dirs first) |
| `GET contents?file=&download=1` | | raw bytes (`download=1` → attachment) |
| `POST write?file=` | raw body | `FileStat` |
| `POST create-directory` | `{name, path}` | `FileStat` |
| `PUT rename` | `{root, files: [{from, to}]}` | `{renamed}` (409 if target exists) |
| `POST copy` | `{location}` | `FileStat` of `name copy.ext` |
| `POST delete` | `{root, files: [..]}` | `{deleted}` |
| `POST compress` | `{root, files: [..], format?: tar.gz\|tar\|zip}` | `FileStat` of `archive-<ts>.<ext>` |
| `POST decompress` | `{root, file}` | `{status, bytes_written}` (tar, tar.gz, zip) |
| `POST chmod` | `{root, files: [{file, mode: "755"}]}` | `{updated}` |
| `POST pull` | `{url, directory, filename?, use_header?, foreground?}` | 202, or `FileStat` if foreground |

`FileStat`: `{name, created, modified, mode: "-rw-r--r--", mode_bits: "644", size, directory, file, symlink, mime}`.

Disk quota → 507, too large → 413. `pull` only allows public http(s)
destinations. Private, loopback, link-local and metadata IPs are refused, and
redirects are re-checked.

### Backups (`/api/servers/{uuid}/backup`)
| Method & path | Body | |
|---|---|---|
| `POST /` | `{adapter: wings\|s3, uuid, ignore: "gitignore lines"}` | 202, result reported to Panel |
| `POST /{backup}/restore` | `{adapter, truncate_directory, download_url?}` | 202. Server must be offline. `download_url` is required for s3 |
| `DELETE /{backup}` | | Deletes a local backup |

`.pteroignore` in the server root is merged into `ignore`.

### Transfers
| Method & path | Body | |
|---|---|---|
| `POST /api/servers/{uuid}/transfer` | `{url: "https://dest/api/transfers", token: "Bearer <jwt>"}` | 202. Source pushes the server |
| `DELETE /api/servers/{uuid}/transfer` | | Cancels an outgoing transfer |
| `POST /api/transfers` (dest, JWT) | raw tar.gz, header `X-Checksum-Sha1` | 200 when extracted |

Sequence: assign the server to the destination in the Panel. Call the source's
`/transfer`. The destination reports `/transfer/success` or `/failure`, and the
source reports `/failure` if the push fails. On success, `DELETE` the server on
the source.

## Browser endpoints (JWT)
| | |
|---|---|
| `GET /api/servers/{uuid}/ws` | Websocket (see below). `Origin` must be `remote` or in `allowed_origins` |
| `GET /download/file?token=` | Single-use file download |
| `GET /download/backup?token=` | Single-use backup download |
| `POST /upload/file?token=&directory=` | multipart, fields named `files`. Per-file limit `api.upload_limit` MiB |
| `PUT /upload/chunk?token=&file=&offset=&total=` | Resumable upload, chunks ≤ 16 MiB, sequential. Returns `{received,total}` until done, then `FileStat`. Wrong offset → 409 `{error, expected_offset}` |

## Websocket protocol

Frames are JSON text: `{"event": "<name>", "args": ["..."]}`.

1. Connect, then send `{"event":"auth","args":["<jwt>"]}` within 30s.
2. The daemon replies `auth success` and then `status <state>`.
3. About 60s before `exp` the daemon sends `token expiring`. Send `auth` again with a fresh token.
   At `exp` it sends `token expired` and stops sending events until you re-authenticate.

| Client → daemon | Args | Permission |
|---|---|---|
| `auth` | `[jwt]` | `websocket.connect` |
| `set state` | `[start\|stop\|restart\|kill]` | `control.<action>` |
| `send command` | `[command]` | `control.console` |
| `send logs` | | `control.console` (replays the buffer as `console output`) |
| `send stats` | | |

| Daemon → client | Args |
|---|---|
| `auth success`, `token expiring`, `token expired` | |
| `jwt error` | `[message]` |
| `status` | `[offline\|starting\|running\|stopping]` |
| `console output` | `[line]` |
| `stats` | `[json ResourceUsage]` (about once per second while running) |
| `daemon message` | `[message]` |
| `daemon error` | `[message]` (detailed only with `admin.websocket.errors`) |
| `install output`, `install started`, `install completed` | (`admin.websocket.install`) |
| `backup completed` | `[json {uuid,is_successful,checksum,checksum_type,file_size}]` (`backup.read`) |
| `backup restore completed` | (`backup.read`) |
| `transfer logs`, `transfer status` | (`admin.websocket.transfer`) |
| `deleted` | then the socket closes |

Clients are limited to 20 messages per second.

## Daemon → Panel (`/api/remote/...`)

| Method & path | Body → Response |
|---|---|
| `GET /servers?page=&per_page=` | → `{data: [ServerConfiguration], meta: {current_page, last_page}}` |
| `GET /servers/{uuid}` | → `ServerConfiguration` |
| `GET /servers/{uuid}/install` | → `{container_image, entrypoint, script}` |
| `POST /servers/{uuid}/install` | `{successful, reinstall}` |
| `POST /servers/reset` | Sent on boot: clear stale installing/restoring flags |
| `POST /servers/states` **ext** | `{data: [{uuid, state, is_installing, is_transferring, is_restoring, utilization}]}` every `state_report_interval` s |
| `POST /servers/{uuid}/events` **ext** | `{event: "crashed", exit_code, oom_killed, restarting, timestamp}` |
| `POST /sftp/auth` | `{type: password\|public_key, username, password, ip}` → `{server, user, permissions}`. 4xx = denied. For `public_key`, `password` holds the OpenSSH public key line |
| `GET /backups/{uuid}?size=` | → `{parts: [presigned PUT urls], part_size}` |
| `POST /backups/{uuid}` | `{checksum, checksum_type: "sha1", size, successful, parts: [{etag, part_number}]}` |
| `POST /backups/{uuid}/restore` | `{successful}` |
| `POST /servers/{uuid}/transfer/success\|failure` | |
| `POST /activity` | `{data: [{user, server, event, metadata, ip, timestamp}]}` (batched every 10s) |

### ServerConfiguration
```json
{
  "settings": {
    "uuid": "8d3f2a1c-...",
    "meta": {"name": "", "description": ""},
    "suspended": false,
    "invocation": "java -Xmx1024M -jar server.jar",
    "skip_egg_scripts": false,
    "environment": {"SERVER_JARFILE": "server.jar"},
    "allocations": {"force_outgoing_ip": false,
                    "default": {"ip": "0.0.0.0", "port": 25565},
                    "mappings": {"0.0.0.0": [25565, 25566]}},
    "build": {"memory_limit": 1024, "swap": 0, "io_weight": 500, "cpu_limit": 100,
              "threads": null, "disk_space": 5120, "oom_disabled": false},
    "crash_detection_enabled": true,
    "mounts": [{"source": "/host/path", "target": "/mnt/x", "read_only": true}],
    "egg": {"id": "...", "file_denylist": ["*.jar"]},
    "container": {"image": "ghcr.io/pterodactyl/yolks:java_21"}
  },
  "process_configuration": {
    "startup": {"done": ["Done ("], "strip_ansi": true},
    "stop": {"type": "command", "value": "stop"},
    "configs": [{"file": "server.properties", "parser": "properties",
                 "replace": [{"match": "server-port", "replace_with": "{{server.build.default.port}}"}]}]
  }
}
```
Units: memory, swap and disk are in MiB. `cpu_limit` is a percent of one core
(0 = unlimited). `swap: -1` means unlimited. `stop.type` is `command`, `signal`
(e.g. `SIGINT` or `^C`) or `stop`.

Container environment: `STARTUP`, `SERVER_MEMORY`, `SERVER_IP`, `SERVER_PORT`,
`P_SERVER_UUID`, `TZ`, plus `environment`. The volume is mounted at
`/home/container`.

### Config file parsers
| parser | `match` syntax |
|---|---|
| `properties` | property name. Missing keys are appended |
| `file` | line prefix. The whole line is replaced |
| `json`, `yaml`, `toml` | dotted path: `server.port`, `listeners.0.host`, `listeners.*.host`. Value types are kept |
| `ini` | `section.key` or `key` |
| `xml` | `root.child.leaf`, `child.@attribute`, `*` wildcard |

`if_value` replaces only when the current value equals it. Placeholders in
`replace_with`:
- `{{server.build.default.port}}`, `{{server.build.default.ip}}`, `{{server.build.memory}}`
- `{{server.build.env.X}}`, `{{env.X}}`
- `{{config.docker.network.interface}}` and other config values (tokens are never exposed)

### SFTP
Port `system.sftp.bind_port` (default 2022). The username is
`<panel username>.<first 8 chars of server uuid>`. Permissions:
`file.read`, `file.read-content`, `file.create`, `file.update`, `file.delete`.
Only the sftp subsystem is available. An IP is blocked for 5 min after 10
failed logins.
