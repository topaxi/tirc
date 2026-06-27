# Local Matrix homeserver for development

A throwaway [Conduit](https://conduit.rs/) homeserver for testing the Matrix
backend against **unencrypted** rooms. Never expose this instance; it has open
registration.

## Start

```bash
docker compose -f dev/matrix/docker-compose.yml up -d
```

Conduit listens on `http://localhost:6167` with server name `localhost`, so user
ids look like `@alice:localhost`.

## Create users

```bash
chmod +x dev/matrix/register.sh
./dev/matrix/register.sh alice alicepassword
./dev/matrix/register.sh bob   bobpassword
```

## Point tirc at it

Add a Matrix entry to `~/.config/tirc/init.lua`:

```lua
config.servers = {
  {
    protocol = 'matrix',
    homeserver = 'http://localhost:6167',
    user_id = '@alice:localhost',
    password = 'alicepassword',
  },
}
```

A room created and joined by both users (e.g. via another client or a second
tirc instance with bob) should then appear as a buffer; messages, membership and
topic changes render through the normalized theme. To create/join a room you can
use the `/j` command once joined, or create one with another Matrix client.

## Stop / reset

```bash
docker compose -f dev/matrix/docker-compose.yml down        # stop
docker compose -f dev/matrix/docker-compose.yml down -v     # stop + wipe data
```

E2E-encrypted rooms are out of scope for now; create rooms with encryption
disabled.
