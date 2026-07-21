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

`alice` (matching `tirc.dev`) is registered on Conduit automatically, by the `up`d
`register` service in docker-compose.yml.

continuwuity has no equivalent: it gates the *first* account on a fresh database
behind a random one-time token, which it only ever prints to its own logs - there is
no way to script around this. So that one account needs a manual step once, right
after `docker compose up`:

```bash
# strip ANSI color codes (the logs have them even though this isn't a tty)
TOKEN=$(docker logs tirc-homeserver-sss 2>&1 | sed -r 's/\x1b\[[0-9;]*m//g' \
  | grep -oP 'using the registration token \K\S+')

curl -s -X POST http://localhost:6168/_matrix/client/v3/register \
  -H 'Content-Type: application/json' \
  -d "{\"username\":\"alice\",\"password\":\"alicepassword\",\"inhibit_login\":true,\"auth\":{\"type\":\"m.login.registration_token\",\"token\":\"${TOKEN}\"}}"
```

After that, `register.sh` works normally on continuwuity too (it uses the *configured*
token, `CONTINUWUITY_REGISTRATION_TOKEN` in docker-compose.yml, which only takes over
once the first account exists). Use it for any additional users on either homeserver:

```bash
chmod +x dev/matrix/register.sh
./dev/matrix/register.sh bob bobpassword                                    # Conduit
HOMESERVER=http://localhost:6168 ./dev/matrix/register.sh bob bobpassword   # continuwuity
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

After `down -v` (or otherwise recreating an account on the same homeserver URL), also
clear tirc's own local state for it - it persists login session and E2E crypto state
keyed by `user id + homeserver` under `~/.local/share/tirc/matrix/`, and a wiped
account gets a new server-side identity that the old local crypto store won't match
(surfaces as `failed to read or write to the crypto store` on connect):

```bash
rm -rf ~/.local/share/tirc/matrix/_alice_localhost@*
```

E2E-encrypted rooms work: the SDK persists its crypto state in the per-account
sqlite store, sends are auto-encrypted, and incoming events are decrypted when
the keys are available. A freshly-logged-in session is unverified, so messages
from senders that only share keys with verified devices show as `[unable to
decrypt ...]` until you verify this session from another client. Interactive
(SAS) verification from within tirc is not implemented yet.
