#!/usr/bin/env bash
# Creates a test team and user on the local Mattermost preview server.
#
# Run after the container is up and healthy:
#
#   docker compose -f dev/mattermost/docker-compose.yml up -d
#   ./dev/mattermost/setup.sh
#
# Optionally override defaults:
#
#   MM_URL=http://localhost:8065 \
#   TEAM_NAME=myteam \
#   TEST_USER=bob TEST_PASS=bobpassword1! \
#   ./dev/mattermost/setup.sh
set -euo pipefail

MM_URL="${MM_URL:-http://localhost:8065}"
ADMIN_USER="${ADMIN_USER:-admin}"
ADMIN_PASS="${ADMIN_PASS:-adminpassword1!}"
TEAM_NAME="${TEAM_NAME:-testteam}"
TEAM_DISPLAY="${TEAM_DISPLAY:-Test Team}"
TEST_USER="${TEST_USER:-alice}"
TEST_PASS="${TEST_PASS:-alicepassword1!}"
TEST_EMAIL="${TEST_EMAIL:-alice@example.com}"

# Wait for the server to be ready.
echo "Waiting for Mattermost at ${MM_URL} ..."
for i in $(seq 1 30); do
  if curl -sf "${MM_URL}/api/v4/system/ping" >/dev/null 2>&1; then
    break
  fi
  if [ "$i" -eq 30 ]; then
    echo "Mattermost did not become ready in time" >&2
    exit 1
  fi
  sleep 2
done
echo "Server is up."

# Log in as admin.
echo "Logging in as ${ADMIN_USER} ..."
login_response=$(curl -sf -X POST "${MM_URL}/api/v4/users/login" \
  -H 'Content-Type: application/json' \
  -D - \
  -d "{\"login_id\":\"${ADMIN_USER}\",\"password\":\"${ADMIN_PASS}\"}")

TOKEN=$(echo "$login_response" | grep -i '^Token:' | tr -d '[:space:]' | cut -d: -f2)
if [ -z "$TOKEN" ]; then
  echo "Failed to extract session token from login response" >&2
  exit 1
fi
echo "Logged in."

auth_header="Authorization: Bearer ${TOKEN}"

# Create the test team (idempotent - ignore 400 if it already exists).
echo "Creating team '${TEAM_NAME}' ..."
curl -sf -X POST "${MM_URL}/api/v4/teams" \
  -H "Content-Type: application/json" \
  -H "${auth_header}" \
  -d "{\"name\":\"${TEAM_NAME}\",\"display_name\":\"${TEAM_DISPLAY}\",\"type\":\"O\"}" \
  > /dev/null || echo "  (team already exists, continuing)"

# Look up the team id.
team_id=$(curl -sf "${MM_URL}/api/v4/teams/name/${TEAM_NAME}" \
  -H "${auth_header}" | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')
echo "Team id: ${team_id}"

# Create the test user (idempotent).
echo "Creating user '${TEST_USER}' ..."
curl -sf -X POST "${MM_URL}/api/v4/users" \
  -H "Content-Type: application/json" \
  -H "${auth_header}" \
  -d "{\"username\":\"${TEST_USER}\",\"password\":\"${TEST_PASS}\",\"email\":\"${TEST_EMAIL}\"}" \
  > /dev/null || echo "  (user already exists, continuing)"

# Look up the user id.
user_id=$(curl -sf "${MM_URL}/api/v4/users/username/${TEST_USER}" \
  -H "${auth_header}" | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')
echo "User id: ${user_id}"

# Add the user to the team.
echo "Adding ${TEST_USER} to team ${TEAM_NAME} ..."
curl -sf -X POST "${MM_URL}/api/v4/teams/${team_id}/members" \
  -H "Content-Type: application/json" \
  -H "${auth_header}" \
  -d "{\"team_id\":\"${team_id}\",\"user_id\":\"${user_id}\"}" \
  > /dev/null || echo "  (already a member, continuing)"

echo ""
echo "Done. Connect tirc with:"
echo ""
echo "  protocol = 'mattermost',"
echo "  url      = '${MM_URL}',"
echo "  user_id  = '${TEST_USER}',"
echo "  password = '${TEST_PASS}',"
echo "  team     = '${TEAM_NAME}',"
echo "  autojoin = { 'town-square' },"
