#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"
umask 077

: "${MM_URL:?Set MM_URL to the HTTPS URL of your test Mattermost server}"
: "${SSH_HOST:?Set SSH_HOST to the SSH destination for your test server}"
: "${APP_CONTAINER:?Set APP_CONTAINER to the Mattermost Docker container name}"
if [[ ! "$MM_URL" =~ ^https://[^/@?#[:space:]]+(/[^?#[:space:]]*)?$ ]]; then
  printf 'MM_URL must be an HTTPS URL without credentials, a query, or a fragment.\n' >&2
  exit 1
fi
if [[ ! "$APP_CONTAINER" =~ ^[a-zA-Z0-9][a-zA-Z0-9_.-]*$ ]]; then
  printf 'APP_CONTAINER must be a Docker container name or ID.\n' >&2
  exit 1
fi
BASE_URL=${MM_URL%/}
CREDS=${CREDS:-.lab/proxmox-credentials.json}
ENV_FILE=${ENV_FILE:-.env.proxmox}
TEAM=acp-lab
CHANNEL=acp-queries
mkdir -p .lab

[[ -f "$CREDS" ]] || printf '{"base_url":"%s","users":{}}\n' "$BASE_URL" > "$CREDS"
chmod 600 "$CREDS"

token_from_headers() {
  awk 'tolower($1)=="token:" {gsub("\r", "", $2); print $2; exit}' "$1"
}

api() {
  local method=$1 path=$2 body=${3:-} token=${TOKEN:-} code
  code=$(curl --proto '=https' -sS --max-time 20 -o .lab/proxmox-response.json -w '%{http_code}' \
    -X "$method" -H "Authorization: Bearer $token" -H 'Content-Type: application/json' \
    ${body:+--data-binary "$body"} "$BASE_URL/api/v4$path")
  if [[ "$code" != 2* ]]; then
    printf 'API %s %s failed: HTTP %s\n' "$method" "$path" "$code" >&2
    jq '{id,message,status_code}' .lab/proxmox-response.json >&2 || true
    return 1
  fi
  cat .lab/proxmox-response.json
}

save_creds() {
  jq "$@" "$CREDS" > .lab/proxmox-credentials.next.json
  mv .lab/proxmox-credentials.next.json "$CREDS"
  chmod 600 "$CREDS"
}

ensure_password() {
  local name=$1
  if ! jq -e --arg n "$name" '.users[$n].password' "$CREDS" >/dev/null; then
    save_creds --arg n "$name" --arg p "Lab9!$(openssl rand -hex 16)" \
      '.users[$n] = ((.users[$n] // {}) + {password:$p})'
  fi
  jq -r --arg n "$name" '.users[$n].password' "$CREDS"
}

login() {
  local name=$1 password=$2 body code
  body=$(jq -nc --arg l "$name" --arg p "$password" '{login_id:$l,password:$p}')
  code=$(curl --proto '=https' -sS --max-time 20 -D .lab/proxmox-login.headers -o .lab/proxmox-login.json -w '%{http_code}' \
    -X POST -H 'Content-Type: application/json' --data-binary "$body" "$BASE_URL/api/v4/users/login")
  [[ "$code" == 200 ]] || return 1
  save_creds --arg n "$name" --arg t "$(token_from_headers .lab/proxmox-login.headers)" \
    --arg i "$(jq -r '.id' .lab/proxmox-login.json)" \
    '.users[$n] += {token:$t,id:$i}'
}

admin_password=$(ensure_password admin)
if ! login admin "$admin_password"; then
  ssh -- "$SSH_HOST" "docker exec $APP_CONTAINER mmctl --local user create \
    --username admin --password '$admin_password' --email admin@acp-lab.invalid --system-admin" \
    >/dev/null 2>&1 || true
  login admin "$admin_password"
fi
TOKEN=$(jq -r '.users.admin.token' "$CREDS")

for name in owner1 owner2; do
  password=$(ensure_password "$name")
  if ! login "$name" "$password"; then
    body=$(jq -nc --arg u "$name" --arg p "$password" --arg e "$name@acp-lab.invalid" \
      '{username:$u,password:$p,email:$e}')
    TOKEN=$(jq -r '.users.admin.token' "$CREDS") api POST /users "$body" >/dev/null
    login "$name" "$password"
  fi
  TOKEN=$(jq -r '.users.admin.token' "$CREDS")
  uid=$(jq -r --arg n "$name" '.users[$n].id' "$CREDS")
  api PUT "/users/$uid/roles" '{"roles":"system_user system_user_access_token"}' >/dev/null || true
  if ! jq -e --arg n "$name" '.users[$n].pat' "$CREDS" >/dev/null; then
    owner_token=$(jq -r --arg n "$name" '.users[$n].token' "$CREDS")
    PAT=$(TOKEN="$owner_token" api POST "/users/$uid/tokens" \
      '{"description":"acp-assist personal access token"}' | jq -r '.token')
    save_creds --arg n "$name" --arg t "$PAT" '.users[$n].pat=$t'
  fi
done

TOKEN=$(jq -r '.users.admin.token' "$CREDS")
api GET /teams > .lab/proxmox-teams.json
team_id=$(jq -r --arg t "$TEAM" '.[]|select(.name==$t)|.id' .lab/proxmox-teams.json)
if [[ -z "$team_id" ]]; then
  team_id=$(api POST /teams "$(jq -nc --arg n "$TEAM" '{name:$n,display_name:"ACP Lab",type:"O"}')" | jq -r '.id')
fi

for name in owner1 owner2; do
  uid=$(jq -r --arg n "$name" '.users[$n].id' "$CREDS")
  api POST "/teams/$team_id/members" "$(jq -nc --arg t "$team_id" --arg u "$uid" '{team_id:$t,user_id:$u}')" >/dev/null || true
done

channel_id=$(api GET "/teams/$team_id/channels/name/$CHANNEL" 2>/dev/null | jq -r '.id // empty' || true)
if [[ -z "$channel_id" ]]; then
  channel_id=$(api POST /channels "$(jq -nc --arg t "$team_id" --arg n "$CHANNEL" '{team_id:$t,name:$n,display_name:"ACP Queries",type:"O"}')" | jq -r '.id')
fi
for name in owner1 owner2; do
  uid=$(jq -r --arg n "$name" '.users[$n].id' "$CREDS")
  api POST "/channels/$channel_id/members" "$(jq -nc --arg u "$uid" '{user_id:$u}')" >/dev/null || true
done

owner1_pat=$(jq -r '.users.owner1.pat' "$CREDS")
owner2_pat=$(jq -r '.users.owner2.pat' "$CREDS")
owner1_id=$(jq -r '.users.owner1.id' "$CREDS")
owner2_id=$(jq -r '.users.owner2.id' "$CREDS")
cat > "$ENV_FILE" <<EOF
MM_URL=$BASE_URL
MM_TEAM=$TEAM
MM_CHANNEL=$CHANNEL
MM_CHANNEL_ID=$channel_id
MM_MONITOR_CHANNEL_IDS=$channel_id
MM_READER_TOKEN=$owner1_pat
MM_OWNER1_TOKEN=$owner1_pat
MM_OWNER1_USER_ID=$owner1_id
MM_OWNER2_TOKEN=$owner2_pat
MM_OWNER2_USER_ID=$owner2_id
TRIGGER_EMOJI=robot,robot_face
EOF
chmod 600 "$ENV_FILE"
printf 'Seeded %s team=%s channel=%s\n' "$BASE_URL" "$team_id" "$channel_id"
printf 'Wrote %s and %s\n' "$ENV_FILE" "$CREDS"
