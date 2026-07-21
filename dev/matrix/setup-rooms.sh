#!/usr/bin/env bash
# Sets up the dev rooms: a local room on each homeserver (native to that
# server, not federated) plus one shared room federated between both. Safe
# to re-run - every room is created idempotently by resolving its alias
# first.
#
#   ./dev/matrix/setup-rooms.sh
#
# Requires both dev homeservers up and BOTH alice accounts already
# registered - including continuwuity's, which needs the one-time manual
# bootstrap step from README.md first.
set -euo pipefail

CA="${CACERT:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/tls/ca.cert.pem}"

login() {
  curl -sS --cacert "$CA" -X POST "$1/_matrix/client/v3/login" \
    -d "{\"type\":\"m.login.password\",\"identifier\":{\"type\":\"m.id.user\",\"user\":\"alice\"},\"password\":\"alicepassword\"}" \
    | sed -n 's/.*"access_token":"\([^"]*\)".*/\1/p'
}

resolve() {
  curl -sS --cacert "$CA" "$1/_matrix/client/v3/directory/room/$2" \
    | sed -n 's/.*"room_id":"\([^"]*\)".*/\1/p'
}

# Creates alias $2 (URL-encoded) on homeserver $1 as already-authenticated
# user $4, named/localparted $5, if it doesn't already exist ($3 is the
# alias in display form, for logging). Prints the room id either way.
ensure_room() {
  local homeserver="$1" alias_enc="$2" alias_display="$3" token="$4" localpart="$5"
  local room_id
  room_id=$(resolve "$homeserver" "$alias_enc")
  if [ -z "$room_id" ]; then
    echo "Creating ${alias_display} on ${homeserver}..." >&2
    local created
    created=$(curl -sS --cacert "$CA" -X POST "${homeserver}/_matrix/client/v3/createRoom" \
      -H "Authorization: Bearer ${token}" -H 'Content-Type: application/json' \
      -d "{\"name\":\"${localpart}\",\"room_alias_name\":\"${localpart}\",\"preset\":\"public_chat\",\"room_version\":\"10\"}")
    room_id=$(echo "$created" | sed -n 's/.*"room_id":"\([^"]*\)".*/\1/p')
    if [ -z "$room_id" ]; then
      echo "failed to create ${alias_display}: $created" >&2
      exit 1
    fi
  else
    echo "${alias_display} already exists (${room_id})" >&2
  fi
  echo "$room_id"
}

TOK_DENDRITE=$(login https://localhost:8448)
TOK_CONTINUWUITY=$(login https://localhost:8449)

dendrite_local=$(ensure_room https://localhost:8448 '%23tirc-local%3Adendrite.local' '#tirc-local:dendrite.local' "$TOK_DENDRITE" tirc-local)
continuwuity_local=$(ensure_room https://localhost:8449 '%23tirc-local%3Acontinuwuity.local' '#tirc-local:continuwuity.local' "$TOK_CONTINUWUITY" tirc-local)
shared=$(ensure_room https://localhost:8449 '%23tirc-dev%3Acontinuwuity.local' '#tirc-dev:continuwuity.local' "$TOK_CONTINUWUITY" tirc-dev)

echo "Joining #tirc-dev:continuwuity.local from dendrite.local (federation)..." >&2
join=$(curl -sS --cacert "$CA" -X POST "https://localhost:8448/_matrix/client/v3/join/%23tirc-dev%3Acontinuwuity.local" \
  -H "Authorization: Bearer ${TOK_DENDRITE}" -H 'Content-Type: application/json' -d '{}')
echo "$join" | grep -q '"room_id"' || { echo "join failed: $join" >&2; exit 1; }

cat <<EOF

Rooms ready:
  #tirc-local:dendrite.local     (${dendrite_local}) - local to Dendrite
  #tirc-local:continuwuity.local (${continuwuity_local}) - local to continuwuity
  #tirc-dev:continuwuity.local   (${shared}) - shared, federated, joined from both
EOF
