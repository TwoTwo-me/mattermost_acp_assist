#!/usr/bin/env bash
source "$(dirname "$0")/common.sh"
[[ -f "$FIXTURE" && -f "$ENV_FILE" ]] \
  || { printf 'FAIL lab fixtures missing; run bash lab/seed.sh first\n'; exit 1; }
env_get() {
  awk -F= -v key="$1" '$1==key {print substr($0,index($0,"=")+1); exit}' "$ENV_FILE"
}
: > artifacts/environment/verify-result.txt
pass() { printf 'PASS %s\n' "$1"; printf 'PASS %s\n' "$1" >> artifacts/environment/verify-result.txt; }
fail() { printf 'FAIL %s\n' "$1"; exit 1; }
channel=$(jq -r '.channel.id' "$FIXTURE")
owner1=$(env_get MM_OWNER1_USER_ID)
owner2=$(env_get MM_OWNER2_USER_ID)
bot=$(env_get MM_BOT_USER_ID)
t1=$(env_get MM_OWNER1_TOKEN)
t2=$(env_get MM_OWNER2_TOKEN)
tbot=$(env_get MM_BOT_TOKEN)
trigger=$(env_get TRIGGER_EMOJI)
for value in channel owner1 owner2 bot t1 t2 tbot trigger; do
  [[ -n "${!value}" ]] || fail ".env or fixture missing value: $value"
done
code=$(curl -sS --max-time 10 -o artifacts/environment/ping.json -w '%{http_code}' "$BASE_URL/api/v4/system/ping")
[[ "$code" == 200 ]] || fail "system ping returned HTTP $code"
[[ "$(jq -r '.status' artifacts/environment/ping.json)" == OK ]] || fail 'system ping status is not OK'
pass 'system ping 200'
ensure_verify_post() {
  local token=$1 uid=$2 message=$3 id
  id=$(TOKEN=$token find_channel_post "$channel" "$message") \
    || fail "could not list posts in acp-queries"
  if [[ -z "$id" ]]; then
    id=$(TOKEN=$token api POST /posts \
      "$(jq -nc --arg c "$channel" --arg m "$message" '{channel_id:$c,message:$m}')" | jq -r '.id')
  fi
  TOKEN=$token
  api GET "/posts/$id" > ".lab/verify-$uid-post.json" || fail "could not read post $id"
  jq -e --arg u "$uid" --arg c "$channel" '.user_id==$u and .channel_id==$c' \
    ".lab/verify-$uid-post.json" >/dev/null \
    || fail "post $id is not owned by $uid in acp-queries"
  printf '%s' "$id"
}
p1=$(ensure_verify_post "$t1" "$owner1" 'acp-lab verify: owner1 check-in')
pass 'owner1 posted in acp-queries'
p2=$(ensure_verify_post "$t2" "$owner2" 'acp-lab verify: owner2 check-in')
pass 'owner2 posted in acp-queries'
react() {
  local token=$1 uid=$2 post=$3 emoji=$4 code
  code=$(TOKEN=$token request GET "/posts/$post/reactions")
  [[ "$code" == 200 ]] || return 1
  if jq -e --arg e "$emoji" --arg u "$uid" \
    '(. // [])|map(select(.emoji_name==$e and .user_id==$u))|length>0' .lab/response.json >/dev/null; then
    return 0
  fi
  code=$(TOKEN=$token request POST /reactions \
    "$(jq -nc --arg u "$uid" --arg p "$post" --arg e "$emoji" '{user_id:$u,post_id:$p,emoji_name:$e}')")
  [[ "$code" == 201 || "$code" == 200 ]] || return 1
  TOKEN=$token
  code=$(request GET "/posts/$post/reactions")
  [[ "$code" == 200 ]] || return 1
  jq -e --arg e "$emoji" --arg u "$uid" \
    '(. // [])|map(select(.emoji_name==$e and .user_id==$u))|length>0' .lab/response.json >/dev/null
}
react "$t2" "$owner2" "$p1" "$trigger" \
  || fail "owner2 reaction '$trigger' not returned by GET /posts/$p1/reactions"
pass "owner2 reacted '$trigger' to owner1 post"
react "$t1" "$owner1" "$p2" "$trigger" \
  || fail "owner1 reaction '$trigger' not returned by GET /posts/$p2/reactions"
pass "owner1 reacted '$trigger' to owner2 post"
code=$(TOKEN=$tbot request POST /channels/direct "$(jq -nc --arg a "$bot" --arg b "$owner1" '[$a,$b]')")
[[ "$code" == 200 || "$code" == 201 ]] || fail "bot direct channel request returned HTTP $code"
dm=$(jq -r '.id' .lab/response.json)
[[ -n "$dm" && "$dm" != null ]] || fail 'bot direct channel response has no id'
marker='acp-lab bot DM check-in'
msg=$(TOKEN=$tbot find_channel_post "$dm" "$marker") || fail 'could not list bot DM posts'
if [[ -z "$msg" ]]; then
  msg=$(TOKEN=$tbot api POST /posts \
    "$(jq -nc --arg c "$dm" --arg m "$marker" '{channel_id:$c,message:$m}')" | jq -r '.id')
fi
TOKEN=$tbot
api GET "/posts/$msg" > artifacts/environment/bot-dm-post.json || fail "could not read bot DM post $msg"
jq -e --arg m "$marker" --arg u "$bot" --arg c "$dm" '.message==$m and .user_id==$u and .channel_id==$c' \
  artifacts/environment/bot-dm-post.json >/dev/null \
  || fail 'bot DM post does not match the sent message'
pass 'bot direct channel with owner1 and message posted'
