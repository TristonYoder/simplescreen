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

echo "info:     $(req GET /v1/info 200 | jq -c '{slug, version, server_id}')"

group="$(req POST /v1/groups 201 "${auth[@]}" "${json[@]}" -d '{"name":"Smoke '"$RANDOM"'"}' | jq -r .id)"
echo "group:    $group"

file="$(mktemp)"; head -c 4096 /dev/urandom > "$file"
sha="$(shasum -a 256 "$file" | cut -d' ' -f1)"
req PUT "/v1/blobs/$sha" 201 "${auth[@]}" -H "Content-Type: application/octet-stream" --data-binary @"$file" >/dev/null
req PUT "/v1/blobs/$sha" 200 "${auth[@]}" -H "Content-Type: application/octet-stream" --data-binary @"$file" >/dev/null
req HEAD "/v1/blobs/$sha" 200 "${auth[@]}" -I >/dev/null
req GET "/v1/blobs/$sha" 206 "${auth[@]}" -H "Range: bytes=0-9" | wc -c | tr -d ' ' | grep -qx 10
echo "blob:     $sha (uploaded, deduped, ranged)"
rm -f "$file"

media="$(req POST /v1/media 201 "${auth[@]}" "${json[@]}" \
  -d '{"group_id":"'"$group"'","kind":"image","sha256":"'"$sha"'","mime":"image/png","title":"Smoke image"}' | jq -r .id)"
echo "media:    $media"

playlist="$(req POST /v1/playlists 201 "${auth[@]}" "${json[@]}" \
  -d '{"group_id":"'"$group"'","name":"Smoke","items":[{"kind":"image","media_id":"'"$media"'"}]}' | jq -r .id)"
echo "playlist: $playlist"

pairing="$(req POST /v1/pairing 201 "${json[@]}" -d '{"model":"smoke"}')"
code="$(jq -r .code <<<"$pairing")"; pid="$(jq -r .pairing_id <<<"$pairing")"; secret="$(jq -r .secret <<<"$pairing")"
req GET "/v1/pairing/$pid?secret=$secret" 200 | jq -e '.status == "pending"' >/dev/null
screen="$(req POST /v1/pairing/claim 201 "${auth[@]}" "${json[@]}" \
  -d '{"code":"'"$code"'","name":"Smoke TV","group_id":"'"$group"'","playlist_id":"'"$playlist"'"}' | jq -r .id)"
poll="$(req GET "/v1/pairing/$pid?secret=$secret" 200)"
device_token="$(jq -r .device_token <<<"$poll")"
[ "$device_token" != "null" ] || { echo "FAIL no device token" >&2; exit 1; }
req GET "/v1/pairing/$pid?secret=$secret" 200 | jq -e '.device_token == null' >/dev/null
echo "screen:   $screen (paired)"

manifest="$(req GET /v1/device/manifest 200 -H "Authorization: Bearer $device_token")"
jq -e '.assets | length == 1' <<<"$manifest" >/dev/null
echo "manifest: $(jq -c '{screen_name, version: .playlist.version, assets: [.assets[].path]}' <<<"$manifest")"

etag="$(curl -sS -o /dev/null -D - "$BASE/v1/device/manifest" -H "Authorization: Bearer $device_token" \
  | tr -d '\r' | awk 'tolower($1) == "etag:" { print $2 }')"
[ -n "$etag" ] || { echo "FAIL no ETag on /v1/device/manifest" >&2; exit 1; }
req GET /v1/device/manifest 304 -H "Authorization: Bearer $device_token" -H "If-None-Match: $etag" >/dev/null
req DELETE "/v1/screens/$screen" 204 "${auth[@]}" >/dev/null
req DELETE "/v1/playlists/$playlist" 204 "${auth[@]}" >/dev/null
req DELETE "/v1/media/$media" 204 "${auth[@]}" >/dev/null
req DELETE "/v1/groups/$group" 204 "${auth[@]}" >/dev/null
echo "smoke OK"
