#!/usr/bin/env bash
source "$(dirname "$0")/common.sh"
for container in "$PROJECT-app" "$PROJECT-db"; do
  if docker container inspect "$container" >/dev/null 2>&1; then
    docker stop "$container" >/dev/null
  fi
done
printf 'Stopped lab containers. Volumes, credentials and fixtures retained.\n'
