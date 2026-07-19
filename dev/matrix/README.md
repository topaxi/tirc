# Local Matrix homeservers for development

Throwaway homeservers for testing the Matrix backend against both unencrypted
and **E2E-encrypted** rooms. Never expose these instances; they have open
registration.

Two homeservers run so both sync drivers can be exercised:

| Service          | Homeserver     | Port | Sync driver tirc selects          |
| ---------------- | -------------- | ---- | --------------------------------- |
| `homeserver`     | Conduit        | 6167 | classic `/sync` (no MSC4186)      |
| `homeserver-sss` | continuwuity   | 6168 | Simplified Sliding Sync (MSC4186) |

tirc auto-detects sliding-sync support per homeserver; override it with the
`sliding_sync` server option (`'on'` / `'off'` / `'auto'`, default `'auto'`).

## Start

```bash
docker compose -f dev/matrix/docker-compose.yml up -d
```

Both listen with server name `localhost`, so user ids look like
`@alice:localhost`: Conduit on `http://localhost:6167`, continuwuity on
`http://localhost:6168`.

## Create users

Register on whichever homeserver you are testing (the script defaults to
Conduit; set `HOMESERVER` for the sliding-sync one):

```bash
chmod +x dev/matrix/register.sh
./dev/matrix/register.sh alice alicepassword
./dev/matrix/register.sh bob   bobpassword

# continuwuity (sliding sync):
HOMESERVER=http://localhost:6168 ./dev/matrix/register.sh alice alicepassword
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
    -- 'auto' (default) probes the homeserver; 'on'/'off' force the driver.
    -- Point at http://localhost:6168 (continuwuity) to exercise sliding sync.
    sliding_sync = 'auto',
  },
}
```

Rooms you are already joined to appear as named buffers on startup; messages,
membership and topic changes render through the normalized theme. From a
Matrix-focused buffer, `:list` shows the public room directory and `:j <roomid>`
joins a room.

On the sliding-sync homeserver the buffer list paints as rooms stream in from
the windowed room list, and per-room history loads from each room's timeline
rather than an up-front backfill.

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
