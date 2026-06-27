#!/usr/bin/env bash
# Registers a user on the local Conduit homeserver (see docker-compose.yml).
#
#   ./dev/matrix/register.sh <username> <password>
#
# Open registration must be enabled (it is, in the dev compose file).
set -euo pipefail

USERNAME="${1:?usage: register.sh <username> <password>}"
PASSWORD="${2:?usage: register.sh <username> <password>}"
HOMESERVER="${HOMESERVER:-http://localhost:6167}"

curl -fsSL -X POST "${HOMESERVER}/_matrix/client/v3/register" \
  -H 'Content-Type: application/json' \
  -d "{\"username\":\"${USERNAME}\",\"password\":\"${PASSWORD}\",\"auth\":{\"type\":\"m.login.dummy\"},\"inhibit_login\":true}" \
  && echo "registered @${USERNAME}:localhost"
