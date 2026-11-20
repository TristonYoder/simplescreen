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
| `SIMPLESCREEN_TIMEZONE` | host zone, else UTC | IANA zone for group schedules; a group's own timezone overrides it |

## NixOS

```nix
{
  inputs.simplescreen.url = "github:TristonYoder/simplescreen";
  # modules = [ simplescreen.nixosModules.default ];
  services.simplescreen = {
    enable = true;
    openFirewall = true;
    timezone = "America/Chicago";   # defaults to time.timeZone
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

```sh
nix build .#docker   # on a Linux builder
docker load < result
docker run -p 8420:8420 -v screen-data:/data simplescreen:<version>
```

Bonjour needs host networking (`--network host`) to reach the LAN.

## Troubleshooting

- **Clients cannot discover the server:** mDNS (UDP 5353) must be reachable; in Docker use host networking, or enter the URL manually. Check `SIMPLESCREEN_BONJOUR` is not false. Advertisement failures only log a warning.
- **401 with an OIDC token:** `iss` must equal the configured issuer exactly (including trailing slash), `aud` must match the audience or client id, and `SIMPLESCREEN_OIDC_TOKEN_TYPE` must match the token the client sends.
- **Signed in but no groups visible:** the IdP groups claim must contain a value listed in a group's `oidc_bindings`; check `SIMPLESCREEN_OIDC_GROUPS_CLAIM`.
- **Lost bootstrap token:** read `<data dir>/bootstrap-token`, or set `SIMPLESCREEN_BOOTSTRAP_TOKEN_FILE`.
- **Uploads fail with 422 `hash_mismatch`:** the bytes differ from the SHA-256 in the URL; re-upload.
- **Scheduled content starts at the wrong hour:** schedule times are wall-clock in the group's timezone. Check the group's timezone in Studio (Groups), then `SIMPLESCREEN_TIMEZONE` / `services.simplescreen.timezone`. The server refuses to start with an unknown zone name.
- **Schedule doesn't switch while the server is down:** players switch offline only within the 48 h timeline they last fetched, and only to playlists they've finished caching. Let the screen reconnect once after editing the schedule.
- **Screen shows offline:** the device WebSocket (`/v1/ws`) must pass through any reverse proxy with upgrade headers enabled.
- **Verify a running server:** `DATA_DIR=./data scripts/smoke.sh` (needs `curl` and `jq`).
