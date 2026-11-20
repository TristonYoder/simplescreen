#!/usr/bin/env bash
# Smoke test against a live server. Usage: BASE=http://localhost:8420 TOKEN=<bootstrap> scripts/smoke.sh
set -euo pipefail

BASE="${BASE:-http://localhost:8420}"
TOKEN="${TOKEN:-}"
if [ -z "$TOKEN" ] && [ -n "${DATA_DIR:-}" ]; then TOKEN="$(cat "$DATA_DIR/bootstrap-token")"; fi
[ -n "$TOKEN" ] || { echo "set TOKEN or DATA_DIR" >&2; exit 2; }

tmp="$(mktemp)"; trap 'rm -f "$tmp"' EXIT
auth=(-H "Authorization: Bearer $TOKEN")
json=(-H "Content-Type: application/json")

# req METHOD PATH EXPECTED_STATUS [curl args...]; prints body
req() {
  local method="$1" path="$2" want="$3"; shift 3
  local code
  code="$(curl -sS -o "$tmp" -w '%{http_code}' -X "$method" "$BASE$path" "$@")"
  if [ "$code" != "$want" ]; then
    echo "FAIL $method $path: got $code, want $want" >&2; cat "$tmp" >&2; echo >&2; exit 1
  fi
  cat "$tmp"
}

info="$(req GET /v1/info 200)"
echo "info:     $(jq -c '{slug, version, server_id}' <<<"$info")"
org="$(req GET /v1/me/organizations 200 "${auth[@]}" -H "X-Organization: $(jq -r .server_id <<<"$info")")"
jq -e '(.organizations | length == 1) and .can_create == false' <<<"$org" >/dev/null
echo "org:      $(jq -c '.organizations[0] | {name, role}' <<<"$org")"
req GET /v1/teams 404 "${auth[@]}" -H "X-Organization: not-this-org" | jq -e '.code == "organization_not_found"' >/dev/null

team="$(req POST /v1/teams 201 "${auth[@]}" "${json[@]}" -d '{"name":"Smoke '"$RANDOM"'"}' | jq -r .id)"
echo "team:    $team"

file="$(mktemp)"; head -c 4096 /dev/urandom > "$file"
sha="$(shasum -a 256 "$file" | cut -d' ' -f1)"
req PUT "/v1/blobs/$sha" 201 "${auth[@]}" -H "Content-Type: application/octet-stream" --data-binary @"$file" >/dev/null
req PUT "/v1/blobs/$sha" 200 "${auth[@]}" -H "Content-Type: application/octet-stream" --data-binary @"$file" >/dev/null
req HEAD "/v1/blobs/$sha" 200 "${auth[@]}" -I >/dev/null
req GET "/v1/blobs/$sha" 206 "${auth[@]}" -H "Range: bytes=0-9" | wc -c | tr -d ' ' | grep -qx 10
echo "blob:     $sha (uploaded, deduped, ranged)"
rm -f "$file"

media="$(req POST /v1/media 201 "${auth[@]}" "${json[@]}" \
  -d '{"team_id":"'"$team"'","kind":"image","sha256":"'"$sha"'","mime":"image/png","title":"Smoke image"}' | jq -r .id)"
echo "media:    $media"

playlist="$(req POST /v1/playlists 201 "${auth[@]}" "${json[@]}" \
  -d '{"team_id":"'"$team"'","name":"Smoke","items":[{"kind":"image","media_id":"'"$media"'"}]}' | jq -r .id)"
echo "playlist: $playlist"

pairing="$(req POST /v1/pairing 201 "${json[@]}" -d '{"model":"smoke"}')"
code="$(jq -r .code <<<"$pairing")"; pid="$(jq -r .pairing_id <<<"$pairing")"; secret="$(jq -r .secret <<<"$pairing")"
req GET "/v1/pairing/$pid?secret=$secret" 200 | jq -e '.status == "pending"' >/dev/null
screen="$(req POST /v1/pairing/claim 201 "${auth[@]}" "${json[@]}" \
  -d '{"code":"'"$code"'","name":"Smoke TV","team_id":"'"$team"'","playlist_id":"'"$playlist"'"}' | jq -r .id)"
poll="$(req GET "/v1/pairing/$pid?secret=$secret" 200)"
device_token="$(jq -r .device_token <<<"$poll")"
[ "$device_token" != "null" ] || { echo "FAIL no device token" >&2; exit 1; }
req GET "/v1/pairing/$pid?secret=$secret" 200 | jq -e '.device_token == null' >/dev/null
echo "screen:   $screen (paired)"

manifest="$(req GET /v1/device/manifest 200 -H "Authorization: Bearer $device_token")"
jq -e '.assets | length == 1' <<<"$manifest" >/dev/null
echo "manifest: $(jq -c '{screen_name, version: .playlist.version, assets: [.assets[].path]}' <<<"$manifest")"

etag="$(curl -sS -o /dev/null -D - -H "Authorization: Bearer $device_token" "$BASE/v1/device/manifest" | tr -d '\r' | sed -n 's/^[Ee][Tt][Aa][Gg]: //p')"
req GET /v1/device/manifest 304 -H "Authorization: Bearer $device_token" -H "If-None-Match: $etag" >/dev/null
req DELETE "/v1/screens/$screen" 204 "${auth[@]}" >/dev/null
req DELETE "/v1/playlists/$playlist" 204 "${auth[@]}" >/dev/null
req DELETE "/v1/media/$media" 204 "${auth[@]}" >/dev/null
req DELETE "/v1/teams/$team" 204 "${auth[@]}" >/dev/null
echo "smoke OK"
