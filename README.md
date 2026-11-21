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

The server is one organization. Its name and timezone come from the env vars above when set (then Studio shows them as locked), otherwise from first-run setup in Studio (`PATCH /v1/settings`, server admin). People belong to **Teams**; a team's `oidc_bindings` map sign-in groups (the IdP groups claim) to viewer, editor or admin. Server admins (`OIDC_ADMIN_GROUPS` or the bootstrap token) manage teams and settings.

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

Data lives in `/var/lib/simplescreen`. Back up `db.sqlite` and `blobs/` together.

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
- **Scheduled content starts at the wrong hour:** schedule times are wall-clock in the organization's timezone. Check it in Studio's settings, then `SIMPLESCREEN_ORG_TIMEZONE` / `services.simplescreen.orgTimezone`. The server refuses to start with an unknown zone name.
- **Schedule doesn't switch while the server is down:** players switch offline only within the 48 h timeline they last fetched, and only to playlists they've finished caching. Let the screen reconnect once after editing the schedule.
- **Screen shows offline:** the device WebSocket (`/v1/ws`) must pass through any reverse proxy with upgrade headers enabled.
- **Verify a running server:** `DATA_DIR=./data scripts/smoke.sh` (needs `curl` and `jq`).

## License and contributing

AGPL-3.0, see [LICENSE](LICENSE). Contributions need a `git commit -s` sign-off under the terms in [CONTRIBUTING.md](CONTRIBUTING.md).
