#!/usr/bin/env bash
source "$(dirname "$0")/common.sh"
curl -fsS --max-time 5 "$BASE_URL/api/v4/system/ping" >/dev/null \
  || { printf 'Lab is not running; run bash lab/start.sh first.\n' >&2; exit 1; }
append_env_key() {
  local key=$1 value=$2
  grep -q "^$key=" "$ENV_FILE" && return 0
  [[ -n "$value" ]] || return 0
  [[ -z "$(tail -c 1 "$ENV_FILE")" ]] || printf '\n' >> "$ENV_FILE"
  printf '%s=%s\n' "$key" "$value" >> "$ENV_FILE"
}
ensure_env_keys() {
  [[ -f "$ENV_FILE" ]] || return 0
  local channel_id pat
  channel_id=$(awk -F= '$1=="MM_CHANNEL_ID" {print substr($0,index($0,"=")+1); exit}' "$ENV_FILE")
  append_env_key MM_MONITOR_CHANNEL_IDS "$channel_id"
  pat=$(jq -r '.users.owner1.pat // empty' "$CREDS" 2>/dev/null) || pat=
  if [[ -z "$pat" ]]; then
    pat=$(awk -F= '$1=="MM_OWNER1_TOKEN" {print substr($0,index($0,"=")+1); exit}' "$ENV_FILE")
  fi
  append_env_key MM_READER_TOKEN "$pat"
  chmod 600 "$ENV_FILE"
}
ensure_env_keys
if [[ -f "$FIXTURE" && -f "$ENV_FILE" ]]; then
  printf 'Existing fixture and .env found; users, channels and posts left untouched.\n'
  exit 0
fi
if [[ ! -f "$CREDS" ]]; then
  jq -n --arg base "$BASE_URL" '{base_url:$base,users:{}}' > "$CREDS"
  chmod 600 "$CREDS"
fi
for name in admin owner1 owner2; do
  if ! jq -e --arg n "$name" '.users[$n].password' "$CREDS" >/dev/null; then
    save_creds --arg n "$name" --arg p "Acp9!$(openssl rand -hex 16)" \
      '.users[$n]={username:$n,password:$p}'
  fi
done
login() {
  local name=$1 body status token
  body=$(jq -c --arg name "$name" '.users[$name]|{login_id:.username,password}' "$CREDS")
  status=$(curl -sS -D .lab/login.headers -o .lab/login.json -w '%{http_code}' \
    -H 'Content-Type: application/json' --data-binary "$body" "$BASE_URL/api/v4/users/login")
  [[ "$status" == 200 ]] || return 1
  token=$(awk 'tolower($1)=="token:" {gsub("\r", "", $2); print $2}' .lab/login.headers)
  save_creds --arg name "$name" --arg token "$token" \
    --arg id "$(jq -r '.id' .lab/login.json)" '.users[$name] += {token:$token,id:$id}'
}
if ! login admin; then
  docker exec "$PROJECT-app" mmctl --local user create --username admin \
    --password "$(jq -r '.users.admin.password' "$CREDS")" \
    --email admin@acp-lab.invalid --system-admin > .lab/mmctl-admin.txt 2>&1 || true
  if ! login admin; then
    api POST /users "$(jq -c '.users.admin|{username,password,email:"admin@acp-lab.invalid"}' "$CREDS")" > .lab/admin-create.json
    login admin
  fi
fi
TOKEN=$(jq -r '.users.admin.token' "$CREDS")
for name in owner1 owner2; do
  if ! login "$name"; then
    api POST /users "$(jq -c --arg n "$name" '.users[$n]|{username,password,email:($n+"@acp-lab.invalid")}' "$CREDS")" \
      > ".lab/$name-create.json"
    login "$name"
  fi
done
for name in owner1 owner2; do
  if ! jq -e --arg n "$name" '.users[$n].pat' "$CREDS" >/dev/null; then
    uid=$(jq -r --arg n "$name" '.users[$n].id' "$CREDS")
    api POST "/users/$uid/tokens" "$(jq -nc --arg n "$name" '{description:("ACP lab "+$n+" token")}')" \
      > ".lab/$name-pat.json"
    save_creds --arg n "$name" --arg t "$(jq -r '.token' ".lab/$name-pat.json")" \
      '.users[$n].pat=$t'
  fi
done
if ! jq -e '.users.bot.id' "$CREDS" >/dev/null; then
  TOKEN=$(jq -r '.users.admin.token' "$CREDS")
  if [[ "$(request GET /users/username/acp-bot)" == 200 ]]; then
    bot_id=$(jq -r '.id' .lab/response.json)
  else
    bot_id=$(api POST /bots \
      '{"username":"acp-bot","display_name":"ACP Bot","description":"Turns emoji reactions into ACP queries"}' \
      | jq -r '.user_id')
  fi
  [[ -n "$bot_id" && "$bot_id" != null ]] \
    || { printf 'Could not create bot acp-bot\n' >&2; exit 1; }
  save_creds --arg id "$bot_id" '.users.bot={username:"acp-bot",id:$id}'
fi
if ! jq -e '.users.bot.token' "$CREDS" >/dev/null; then
  bot_token=$(docker exec "$PROJECT-app" mmctl --local --json token generate acp-bot \
    "ACP lab bot token" 2>/dev/null | jq -r '.[0].token // empty') || bot_token=
  if [[ -z "$bot_token" ]]; then
    bot_id=$(jq -r '.users.bot.id' "$CREDS")
    TOKEN=$(jq -r '.users.admin.token' "$CREDS")
    api POST "/users/$bot_id/tokens" '{"description":"ACP lab bot token"}' > .lab/bot-pat.json
    bot_token=$(jq -r '.token' .lab/bot-pat.json)
  fi
  save_creds --arg t "$bot_token" '.users.bot.token=$t'
fi
TOKEN=$(jq -r '.users.admin.token' "$CREDS")
api GET /teams > .lab/teams.json
team=$(jq -r '.[]|select(.name=="acp-lab")|.id' .lab/teams.json)
if [[ -z "$team" ]]; then
  team=$(api POST /teams '{"name":"acp-lab","display_name":"ACP Lab","type":"O"}' | jq -r '.id')
fi
ensure_team_member() {
  local uid=$1 code
  code=$(TOKEN=$(jq -r '.users.admin.token' "$CREDS") request GET "/teams/$team/members/$uid")
  [[ "$code" == 200 ]] && return 0
  TOKEN=$(jq -r '.users.admin.token' "$CREDS")
  api POST "/teams/$team/members" "$(jq -nc --arg t "$team" --arg u "$uid" '{team_id:$t,user_id:$u}')" >/dev/null
}
for name in owner1 owner2 bot; do
  ensure_team_member "$(jq -r --arg n "$name" '.users[$n].id' "$CREDS")"
done
TOKEN=$(jq -r '.users.admin.token' "$CREDS")
api GET "/teams/$team/channels" > .lab/channels.json
channel=$(jq -r '.[]|select(.name=="acp-queries")|.id' .lab/channels.json)
if [[ -z "$channel" ]]; then
  channel=$(api POST /channels \
    "$(jq -nc --arg t "$team" '{team_id:$t,name:"acp-queries",display_name:"ACP Queries",type:"O"}')" | jq -r '.id')
fi
ensure_channel_member() {
  local uid=$1 code
  code=$(TOKEN=$(jq -r '.users.admin.token' "$CREDS") request GET "/channels/$channel/members/$uid")
  [[ "$code" == 200 ]] && return 0
  TOKEN=$(jq -r '.users.admin.token' "$CREDS")
  api POST "/channels/$channel/members" "$(jq -nc --arg u "$uid" '{user_id:$u}')" >/dev/null
}
for name in owner1 owner2 bot; do
  ensure_channel_member "$(jq -r --arg n "$name" '.users[$n].id' "$CREDS")"
done
ensure_post() {
  local token=$1 message=$2 id
  id=$(TOKEN=$token find_channel_post "$channel" "$message")
  if [[ -z "$id" ]]; then
    id=$(TOKEN=$token api POST /posts \
      "$(jq -nc --arg c "$channel" --arg m "$message" '{channel_id:$c,message:$m}')" | jq -r '.id')
  fi
  printf '%s' "$id"
}
owner1=$(jq -r '.users.owner1.id' "$CREDS")
owner2=$(jq -r '.users.owner2.id' "$CREDS")
bot_id=$(jq -r '.users.bot.id' "$CREDS")
t1=$(jq -r '.users.owner1.pat' "$CREDS")
t2=$(jq -r '.users.owner2.pat' "$CREDS")
post1=$(ensure_post "$t1" 'ACP lab reaction probe from owner1')
post2=$(ensure_post "$t2" 'ACP lab reaction probe from owner2')
probe_reaction() {
  local token=$1 uid=$2 post=$3 emoji=$4 code
  code=$(TOKEN=$token request POST /reactions \
    "$(jq -nc --arg u "$uid" --arg p "$post" --arg e "$emoji" '{user_id:$u,post_id:$p,emoji_name:$e}')")
  [[ "$code" == 201 || "$code" == 200 ]] || return 1
  TOKEN=$token
  code=$(request GET "/posts/$post/reactions")
  [[ "$code" == 200 ]] || return 1
  jq -e --arg e "$emoji" --arg u "$uid" \
    '(. // [])|map(select(.emoji_name==$e and .user_id==$u))|length>0' .lab/response.json >/dev/null
}
trigger_emoji=
for candidate in robot robot_face eyes; do
  if probe_reaction "$t2" "$owner2" "$post1" "$candidate" \
    && probe_reaction "$t1" "$owner1" "$post2" "$candidate"; then
    TOKEN=$t2
    request GET "/posts/$post1/reactions" >/dev/null
    trigger_emoji=$(jq -r --arg e "$candidate" \
      '(. // [])[]|select(.emoji_name==$e)|.emoji_name' .lab/response.json | awk 'NR==1')
    [[ -n "$trigger_emoji" ]] && break
  fi
done
[[ -n "$trigger_emoji" ]] \
  || { printf 'No usable trigger emoji (tried robot, robot_face, eyes)\n' >&2; exit 1; }
jq -n --arg base "$BASE_URL" --arg team "$team" --arg channel "$channel" \
  --arg o1 "$owner1" --arg o2 "$owner2" --arg bot "$bot_id" \
  --arg p1 "$post1" --arg p2 "$post2" --arg emoji "$trigger_emoji" \
  '{base_url:$base,
    team:{id:$team,name:"acp-lab"},
    channel:{id:$channel,name:"acp-queries"},
    users:{owner1:{id:$o1,username:"owner1"},owner2:{id:$o2,username:"owner2"},bot:{id:$bot,username:"acp-bot"}},
    posts:{owner1_probe:{id:$p1,user_id:$o1},owner2_probe:{id:$p2,user_id:$o2}},
    trigger_emoji:$emoji}' > .lab/fixture.next.json
chmod 600 .lab/fixture.next.json
mv .lab/fixture.next.json "$FIXTURE"
{
  printf 'MM_URL=%s\n' "$BASE_URL"
  printf 'MM_TEAM=acp-lab\n'
  printf 'MM_CHANNEL=acp-queries\n'
  printf 'MM_CHANNEL_ID=%s\n' "$channel"
  printf 'MM_MONITOR_CHANNEL_IDS=%s\n' "$channel"
  printf 'MM_BOT_TOKEN=%s\n' "$(jq -r '.users.bot.token' "$CREDS")"
  printf 'MM_BOT_USER_ID=%s\n' "$bot_id"
  printf 'MM_OWNER1_TOKEN=%s\n' "$t1"
  printf 'MM_OWNER1_USER_ID=%s\n' "$owner1"
  printf 'MM_OWNER2_TOKEN=%s\n' "$t2"
  printf 'MM_OWNER2_USER_ID=%s\n' "$owner2"
  printf 'TRIGGER_EMOJI=%s\n' "$trigger_emoji"
  printf 'MM_READER_TOKEN=%s\n' "$t1"
} > .lab/env.next
chmod 600 .lab/env.next
mv .lab/env.next "$ENV_FILE"
printf 'Seeded team acp-lab, channel acp-queries, users owner1/owner2, bot acp-bot.\n'
printf 'TRIGGER_EMOJI=%s\n' "$trigger_emoji"
