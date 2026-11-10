#!/usr/bin/env bash
source "$(dirname "$0")/common.sh"
if [[ ! -f .lab/postgres.env ]]; then
  printf 'POSTGRES_USER=mmuser\nPOSTGRES_DB=mattermost\nPOSTGRES_PASSWORD=%s\n' "$(openssl rand -hex 24)" > .lab/postgres.env
  chmod 600 .lab/postgres.env
fi
docker network inspect "$PROJECT" >/dev/null 2>&1 || docker network create "$PROJECT" >/dev/null
for volume in postgres config data logs plugins client-plugins bleve; do
  docker volume create "$PROJECT-$volume" >/dev/null
done
if docker container inspect "$PROJECT-db" >/dev/null 2>&1; then
  docker start "$PROJECT-db" >/dev/null
else
  docker run -d --name "$PROJECT-db" --label "$PROJECT"=true --restart unless-stopped \
    --platform linux/arm64 --network "$PROJECT" --env-file .lab/postgres.env \
    -v "$PROJECT-postgres":/var/lib/postgresql/data "$PG_IMAGE" >/dev/null
fi
for ((i=0;i<120;i++)); do
  if docker exec "$PROJECT-db" pg_isready -U mmuser -d mattermost >/dev/null 2>&1; then break; fi
  sleep 1
done
docker exec "$PROJECT-db" pg_isready -U mmuser -d mattermost
if docker container inspect "$PROJECT-app" >/dev/null 2>&1; then
  docker start "$PROJECT-app" >/dev/null
else
  db_password=$(awk -F= '$1=="POSTGRES_PASSWORD"{print substr($0,index($0,"=")+1)}' .lab/postgres.env)
  printf 'MM_SQLSETTINGS_DATASOURCE=postgres://mmuser:%s@%s-db:5432/mattermost?sslmode=disable&connect_timeout=10\n' \
    "$db_password" "$PROJECT" > .lab/mattermost.env
  chmod 600 .lab/mattermost.env
  docker run -d --name "$PROJECT-app" --label "$PROJECT"=true --restart unless-stopped \
    --platform linux/amd64 --network "$PROJECT" -p 127.0.0.1:18066:8065 \
    --env-file .lab/mattermost.env -e MM_SQLSETTINGS_DRIVERNAME=postgres \
    -e MM_SERVICESETTINGS_SITEURL="$BASE_URL" -e MM_SERVICESETTINGS_ENABLELOCALMODE=true \
    -e MM_SERVICESETTINGS_ENABLEUSERACCESSTOKENS=true -e MM_TEAMSETTINGS_ENABLEOPENSERVER=true \
    -e MM_SERVICESETTINGS_ENABLEBOTACCOUNTCREATION=true \
    -e MM_EMAILSETTINGS_SENDEMAILNOTIFICATIONS=false -e MM_EMAILSETTINGS_REQUIREEMAILVERIFICATION=false \
    -e MM_LOGSETTINGS_CONSOLELEVEL=ERROR -e MM_PLUGINSETTINGS_ENABLEMARKETPLACE=false \
    -e MM_SERVICESETTINGS_ENABLESECURITYFIXALERT=false -e MM_LOGSETTINGS_ENABLEDIAGNOSTICS=false \
    -v "$PROJECT-config":/mattermost/config -v "$PROJECT-data":/mattermost/data \
    -v "$PROJECT-logs":/mattermost/logs -v "$PROJECT-plugins":/mattermost/plugins \
    -v "$PROJECT-client-plugins":/mattermost/client/plugins -v "$PROJECT-bleve":/mattermost/bleve-indexes "$MM_IMAGE" >/dev/null
fi
for ((i=0;i<240;i++)); do
  if curl -fsS --max-time 2 "$BASE_URL/api/v4/system/ping" > .lab/ping.json 2>/dev/null; then
    printf 'Mattermost ready: %s\n' "$BASE_URL"
    exit 0
  fi
  sleep 2
done
printf 'Mattermost did not become healthy within 480 seconds\n' >&2
exit 1
