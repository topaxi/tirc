# Local Matrix homeserver for development

A throwaway [Conduit](https://conduit.rs/) homeserver for testing the Matrix
backend against both unencrypted and **E2E-encrypted** rooms. Never expose this
instance; it has open registration.

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

Rooms you are already joined to appear as named buffers on startup; messages,
membership and topic changes render through the normalized theme. From a
Matrix-focused buffer, `:list` shows the public room directory and `:j <roomid>`
joins a room.

Note: Conduit's default room version uses **server-less room ids** (just
`!abc...`, no `:localhost` suffix). Use the exact id returned by `createRoom`;
an over-qualified id (`!abc...:localhost`) will not resolve.

## Stop / reset

```bash
docker compose -f dev/matrix/docker-compose.yml down        # stop
docker compose -f dev/matrix/docker-compose.yml down -v     # stop + wipe data
```

E2E-encrypted rooms work: the SDK persists its crypto state in the per-account
sqlite store, sends are auto-encrypted, and incoming events are decrypted when
the keys are available. A freshly-logged-in session is unverified, so messages
from senders that only share keys with verified devices show as `[unable to
decrypt ...]` until you verify this session from another client. Interactive
(SAS) verification from within tirc is not implemented yet.
