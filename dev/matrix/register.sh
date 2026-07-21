#!/usr/bin/env bash
# Registers a user on a local dev homeserver (Conduit or continuwuity; see
# docker-compose.yml).
#
#   ./dev/matrix/register.sh <username> <password>
#   HOMESERVER=http://localhost:6168 ./dev/matrix/register.sh <username> <password>
#
# Open registration must be enabled (it is, in the dev compose file). Conduit
# completes registration with a plain `m.login.dummy` stage; continuwuity
# requires completing an `m.login.registration_token` stage instead (with
# CONTINUWUITY_REGISTRATION_TOKEN from docker-compose.yml) - this script
# detects which one the target homeserver asks for. continuwuity's *first*
# account is created separately, directly at container boot (see
# docker-compose.yml), since it gates that one behind a random one-time token
# printed to the container's logs rather than the configured one.
set -euo pipefail

USERNAME="${1:?usage: register.sh <username> <password>}"
PASSWORD="${2:?usage: register.sh <username> <password>}"
HOMESERVER="${HOMESERVER:-http://localhost:6167}"
REGISTRATION_TOKEN="${REGISTRATION_TOKEN:-devtoken}"

echo "Waiting for ${HOMESERVER} ..."
for i in $(seq 1 30); do
  if curl -sf "${HOMESERVER}/_matrix/client/versions" >/dev/null 2>&1; then
    break
  fi
  if [ "$i" -eq 30 ]; then
    echo "${HOMESERVER} did not become ready in time" >&2
    exit 1
  fi
  sleep 2
done

# Discovering the required auth stage always 401s with the available flows
# and a session id (User-Interactive Auth) - unless the user already exists,
# which 400s immediately with no session (idempotent no-op in that case).
flows=$(curl -sS -X POST "${HOMESERVER}/_matrix/client/v3/register" \
  -H 'Content-Type: application/json' \
  -d "{\"username\":\"${USERNAME}\",\"password\":\"${PASSWORD}\",\"inhibit_login\":true}")

if echo "$flows" | grep -q 'M_USER_IN_USE'; then
  echo "@${USERNAME}:localhost already registered, skipping"
  exit 0
fi

session=$(echo "$flows" | sed -n 's/.*"session":"\([^"]*\)".*/\1/p')

if echo "$flows" | grep -q 'm.login.registration_token'; then
  auth="{\"type\":\"m.login.registration_token\",\"token\":\"${REGISTRATION_TOKEN}\",\"session\":\"${session}\"}"
else
  auth="{\"type\":\"m.login.dummy\",\"session\":\"${session}\"}"
fi

curl -fsSL -X POST "${HOMESERVER}/_matrix/client/v3/register" \
  -H 'Content-Type: application/json' \
  -d "{\"username\":\"${USERNAME}\",\"password\":\"${PASSWORD}\",\"inhibit_login\":true,\"auth\":${auth}}" \
  && echo "registered @${USERNAME}:localhost"
