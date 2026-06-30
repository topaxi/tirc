# Mattermost dev server

A local Mattermost instance for developing and testing the tirc Mattermost backend.

## Start

```bash
docker compose -f dev/mattermost/docker-compose.yml up -d
./dev/mattermost/setup.sh
```

The setup script waits for the server to be ready, then creates:
- Team: `testteam`
- User: `alice` / `alicepassword1!`

## Configure tirc

Add a server entry to `~/.config/tirc/init.lua`:

```lua
{
  protocol = 'mattermost',
  url      = 'http://localhost:8065',
  user_id  = 'alice',
  password = 'alicepassword1!',
  team     = 'testteam',
  autojoin = { 'town-square' },
}
```

## Customise

Override any default via environment variables before running `setup.sh`:

```bash
MM_URL=http://localhost:8065 \
TEAM_NAME=myteam \
TEST_USER=bob TEST_PASS=bobpassword1! \
./dev/mattermost/setup.sh
```

## Stop / reset

```bash
# Stop, keep data.
docker compose -f dev/mattermost/docker-compose.yml down

# Stop and wipe all data.
docker compose -f dev/mattermost/docker-compose.yml down -v
```
