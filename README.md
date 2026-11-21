# Simple Screen

Digital signage server for Apple TV players. This README covers deployment and troubleshooting only.

Status: alpha. The server speaks plain HTTP; put it behind a TLS reverse proxy for anything beyond a trusted LAN. The Apple TV player and Studio editor apps live in a separate repository.

## Run

```sh
SIMPLESCREEN_DATA_DIR=./data nix run .#server
```

On first start without OIDC, a bootstrap admin token is written to `<data dir>/bootstrap-token` (mode 0600); the log prints its path, never the token.

## Configuration

Environment variables (or a TOML file via `SIMPLESCREEN_CONFIG` using the same keys in lowercase):

| Variable | Default | Notes |
|---|---|---|
| `SIMPLESCREEN_PORT` | 8420 | |
| `SIMPLESCREEN_BIND` | 0.0.0.0 | |
| `SIMPLESCREEN_DATA_DIR` | ./data | `db.sqlite`, `blobs/`, `tmp/` |
| `SIMPLESCREEN_BOOTSTRAP_TOKEN_FILE` | | static server-admin bearer token |
| `SIMPLESCREEN_OIDC_ISSUER` | | enables OIDC sign-in |
| `SIMPLESCREEN_OIDC_CLIENT_ID` | | |
| `SIMPLESCREEN_OIDC_AUDIENCE` | client id | expected `aud` |
| `SIMPLESCREEN_OIDC_GROUPS_CLAIM` | groups | |
| `SIMPLESCREEN_OIDC_ADMIN_GROUPS` | | comma list, grants server admin |
| `SIMPLESCREEN_OIDC_TOKEN_TYPE` | access_token | or `id_token` |
| `SIMPLESCREEN_BONJOUR` | true | mDNS advertisement |
| `SIMPLESCREEN_ORG_NAME` | | organization name shown in the apps and on paired TVs; if unset, set it in Studio's first-run setup |
| `SIMPLESCREEN_ORG_TIMEZONE` | host zone, else UTC | IANA zone for every schedule; if unset, Studio can set it. `SIMPLESCREEN_TIMEZONE` is refused at startup |
| `SIMPLESCREEN_LISTEN` | | `unix:/path/to.sock` listens on a unix socket instead of `BIND`:`PORT` (no Bonjour) |
| `SIMPLESCREEN_PUBLIC_URL` | | externally reachable base URL when behind a reverse proxy; reported in `/v1/info` |
| `SIMPLESCREEN_LIBRARY_ROOTS` | | comma-separated absolute directories that local media libraries may live under |

The server is one organization. Its name and timezone come from the env vars above when set (then Studio shows them as locked), otherwise from first-run setup in Studio (`PATCH /v1/settings`, server admin). People belong to **Teams**; a team's `oidc_bindings` map sign-in groups (the IdP groups claim) to viewer, editor or admin. Server admins (`OIDC_ADMIN_GROUPS` or the bootstrap token) manage teams and settings. Screens, devices and floor plans belong to the organization; teams own content. Server admins tag screens and give each team the tags it reaches, and a team's playlists show on a screen only when they share a tag with it. An untagged screen plays nothing until an admin tags it.

Upgrading to this release drops existing screens, devices and schedules: re-pair every device after the upgrade.

### Media libraries

Media lives in folders of **libraries**, which server admins add in Studio:

- **Library** (managed): the server's own store in `<data dir>/blobs`. It always exists.
- **Local:** a directory on the server, inside one of `SIMPLESCREEN_LIBRARY_ROOTS`. Folders and files are real, but change them only through the apps: the server doesn't watch the directory.
- **S3:** a bucket (AWS or any S3-compatible store) with an optional key prefix and CDN URL. Players download from S3 or the CDN directly, so they need internet access to it. Studio uploads straight to the bucket, in parts for files over the part size (64 MiB by default, up to S3's 5 TiB per file). Add a lifecycle rule that aborts incomplete multipart uploads after a few days; the server also aborts its own after 24 hours. The bucket's CORS settings don't matter (the apps aren't browsers). The access key needs `s3:GetObject`, `s3:PutObject`, `s3:DeleteObject`, `s3:AbortMultipartUpload` and `s3:ListBucket` on the prefix.

Upgrading to this release also drops existing media and empties playlists: upload the files again into library folders.

## NixOS

```nix
{
  inputs.simplescreen.url = "github:TristonYoder/simplescreen";
  # modules = [ simplescreen.nixosModules.default ];
  services.simplescreen = {
    enable = true;
    openFirewall = true;
    orgName = "Grace Church";       # null = set in Studio's first-run setup
    orgTimezone = "America/Chicago"; # null = host zone (time.timeZone), editable in Studio
    bootstrapTokenFile = "/run/secrets/simplescreen-token";
    oidc = {
      issuer = "https://id.example.com/application/o/screen/";
      clientId = "screen";
      adminGroups = [ "screen-admins" ];
    };
  };
}
```

Data lives in `/var/lib/simplescreen`. Back up `db.sqlite` and `blobs/` together, along with any local library directories.

For local libraries, list their parent directories in `libraryRoots = [ "/srv/signage" ];`. The service runs with `DynamicUser`, so give the directory a group the service can write to (or put it under `dataDir`). `ProtectHome` blocks directories under `/home`.

## Docker

CI publishes a multi-arch image (amd64, arm64) to GHCR on every push to `main` (`latest`) and on `v*` tags (the version without the `v`).

```sh
docker run -d --name simplescreen -p 8420:8420 \
  -e SIMPLESCREEN_ORG_TIMEZONE=America/Chicago \
  -v simplescreen-data:/data ghcr.io/tristonyoder/simplescreen:latest
docker cp simplescreen:/data/bootstrap-token .   # first-run admin token
```

- Configuration is the same `SIMPLESCREEN_*` environment as above. Set `SIMPLESCREEN_ORG_TIMEZONE` (or set the timezone in Studio's first-run setup), since the container has no local timezone and defaults to UTC.
- The server runs as uid 1000. A named volume gets the right owner automatically. For a bind mount, `chown 1000:1000` the host directory first.
- The image has no shell, so read files with `docker cp`, not `docker exec`.
- For a token you provide, mount it and set `SIMPLESCREEN_BOOTSTRAP_TOKEN_FILE`.
- Bonjour needs host networking (`--network host`) to reach the LAN. Otherwise set `SIMPLESCREEN_BONJOUR=false` and enter the URL in the apps.

To build it yourself on a Linux builder: `nix build .#docker && docker load < result`.

## Troubleshooting

- **Clients cannot discover the server:** mDNS (UDP 5353) must be reachable; in Docker use host networking, or enter the URL manually. Check `SIMPLESCREEN_BONJOUR` is not false. Advertisement failures only log a warning.
- **401 with an OIDC token:** `iss` must equal the configured issuer exactly (including trailing slash), `aud` must match the audience or client id, and `SIMPLESCREEN_OIDC_TOKEN_TYPE` must match the token the client sends.
- **Signed in but no teams visible:** the IdP groups claim must contain a value listed in a team's `oidc_bindings`; check `SIMPLESCREEN_OIDC_GROUPS_CLAIM`.
- **404 `organization_not_found`:** the client sent an `X-Organization` header for a different server; sign in again.
- **Lost bootstrap token:** read `<data dir>/bootstrap-token`, or set `SIMPLESCREEN_BOOTSTRAP_TOKEN_FILE`.
- **Uploads fail with 422 `hash_mismatch`:** the bytes differ from the SHA-256 in the URL; re-upload.
- **Adding a local library fails with `path_not_allowed`:** the path must be inside `SIMPLESCREEN_LIBRARY_ROOTS` (or `libraryRoots`).
- **Adding a library fails with `storage_unreachable`:** the server couldn't write, read back and delete a test file. The message has the reason: check directory permissions, or the S3 endpoint, region, bucket, path-style setting and key permissions.
- **Uploads fail with 507 `insufficient_storage`:** the library's quota or disk is full (`free_bytes` in the error).
- **Players don't download from an S3 library:** they're redirected to S3 or the CDN URL; that host must be reachable from the TVs' network.
- **Scheduled content starts at the wrong hour:** schedule times are wall-clock in the organization's timezone. Check it in Studio's settings, then `SIMPLESCREEN_ORG_TIMEZONE` / `services.simplescreen.orgTimezone`. The server refuses to start with an unknown zone name.
- **Schedule doesn't switch while the server is down:** players switch offline only within the 48 h timeline they last fetched, and only to playlists they've finished caching. Let the screen reconnect once after editing the schedule.
- **Screen shows offline:** the device WebSocket (`/v1/ws`) must pass through any reverse proxy with upgrade headers enabled.
- **Verify a running server:** `DATA_DIR=./data scripts/smoke.sh` (needs `curl` and `jq`).

## License and contributing

AGPL-3.0, see [LICENSE](LICENSE). Contributions need a `git commit -s` sign-off under the terms in [CONTRIBUTING.md](CONTRIBUTING.md).
