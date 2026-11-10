#!/usr/bin/env bash
set -euo pipefail
LAB_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$LAB_ROOT"
umask 077
mkdir -p .lab artifacts/environment
BASE_URL=http://127.0.0.1:18066
PROJECT=mm-acp-lab
CREDS=.lab/credentials.json
FIXTURE=.lab/fixture.json
ENV_FILE=.env
MM_IMAGE=mattermost/mattermost-team-edition:11.11.1@sha256:6ad5912b45875ef2fafc41886b5a70a3ec1b6c17ec706e0492566e8a2fd8aa85
PG_IMAGE=postgres:17-alpine@sha256:b0f9560a2de083e2cc7382e75f808c7381a32852a7ec49117deedb300e552b24
request() {
  local method=$1 path=$2 body=${3:-}
  curl -sS --connect-timeout 5 --max-time 30 -o .lab/response.json -w '%{http_code}' \
    -X "$method" -H "Authorization: Bearer ${TOKEN:-}" -H 'Content-Type: application/json' \
    ${body:+--data-binary "$body"} "$BASE_URL/api/v4$path"
}
api() {
  local method=$1 path=$2 body=${3:-} code
  code=$(request "$method" "$path" "$body")
  if [[ "$code" != 2* ]]; then
    printf 'API %s %s failed: HTTP %s\n' "$method" "$path" "$code" >&2
    jq '{id,message,status_code}' .lab/response.json >&2 || true
    return 1
  fi
  cat .lab/response.json
}
save_creds() {
  jq "$@" "$CREDS" > .lab/credentials.next.json
  mv .lab/credentials.next.json "$CREDS"
  chmod 600 "$CREDS"
}
find_channel_post() {
  local channel=$1 message=$2
  api GET "/channels/$channel/posts?per_page=200" > .lab/channel-posts.json
  jq -r --arg m "$message" '[ (.posts // {})[] | select(.message==$m)][0].id // empty' .lab/channel-posts.json
}
