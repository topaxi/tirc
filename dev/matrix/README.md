# Local Matrix homeservers for development

Throwaway homeservers for testing the Matrix backend against both unencrypted
and **E2E-encrypted** rooms, federated with each other so a room hosted on one
is joinable - and chattable - from the other. Never expose these instances;
they have open registration.

Two homeservers run so both sync drivers can be exercised:

| Service          | Homeserver   | server_name        | Port | Sync driver tirc selects          |
| ---------------- | ------------ | ------------------- | ---- | ---------------------------------- |
| `homeserver`     | Dendrite     | `dendrite.local`    | 8448 | classic `/sync` (no MSC4186)      |
| `homeserver-sss` | continuwuity | `continuwuity.local` | 8449 | Simplified Sliding Sync (MSC4186) |

tirc auto-detects sliding-sync support per homeserver; override it with the
`sliding_sync` server option (`'on'` / `'off'` / `'auto'`, default `'auto'`).

Federation needs real TLS (the Matrix federation API is always HTTPS), so both
homeservers serve their single combined client+federation port over TLS using
a throwaway CA committed under `tls/`. Point tirc's `root_ca_pem` server
option at `tls/ca.cert.pem` to trust it - `dev.lua`'s `matrix`/`matrix_sliding`
entries already do this for you.

An earlier version of this setup paired continuwuity with Conduit instead of
Dendrite. Conduit is unmaintained and rejects every federation request signed
by continuwuity with a deterministic signature-verification error (reproduced
with a fresh minimal room and a retried send - not a timing fluke), so
continuwuity users' messages could never reach it in either room-hosting
direction. Dendrite is matrix.org's own actively maintained classic-sync
implementation and federates cleanly with continuwuity both ways - verified
with real client-server message round trips through tirc's own Matrix
backend, not just curl.

## Start

```bash
docker compose -f dev/matrix/docker-compose.yml up -d
```

## Create users

`alice` (matching `tirc.dev`) is registered on Dendrite automatically, by the
`up`d `register` service in docker-compose.yml.

continuwuity has no equivalent: it gates the *first* account on a fresh
database behind a random one-time token, which it only ever prints to its own
logs - there is no way to script around this. So that one account needs a
manual step once, right after `docker compose up`:

```bash
# strip ANSI color codes (the logs have them even though this isn't a tty)
TOKEN=$(docker logs tirc-homeserver-sss 2>&1 | sed -r 's/\x1b\[[0-9;]*m//g' \
  | grep -oP 'using the registration token \K\S+')

curl -s --cacert dev/matrix/tls/ca.cert.pem -X POST https://localhost:8449/_matrix/client/v3/register \
  -H 'Content-Type: application/json' \
  -d "{\"username\":\"alice\",\"password\":\"alicepassword\",\"inhibit_login\":true,\"auth\":{\"type\":\"m.login.registration_token\",\"token\":\"${TOKEN}\"}}"
```

After that, `register.sh` works normally on continuwuity too (it uses the
*configured* token, `CONTINUWUITY_REGISTRATION_TOKEN` in docker-compose.yml,
which only takes over once the first account exists). Use it for any
additional users on either homeserver:

```bash
chmod +x dev/matrix/register.sh
./dev/matrix/register.sh bob bobpassword                                    # Dendrite
HOMESERVER=https://localhost:8449 ./dev/matrix/register.sh bob bobpassword  # continuwuity
```

## Create the dev rooms

Once continuwuity's `alice` exists (the manual step above), set up the dev
rooms both `dev.lua` entries autojoin:

```bash
chmod +x dev/matrix/setup-rooms.sh
./dev/matrix/setup-rooms.sh
```

Idempotent - safe to re-run. It creates three rooms (if they don't exist yet):

- `#tirc-local:dendrite.local` - local to Dendrite, not federated
- `#tirc-local:continuwuity.local` - local to continuwuity, not federated
- `#tirc-dev:continuwuity.local` - shared, joined from `dendrite.local` via
  federation, so both alice accounts can chat across homeservers

## Point tirc at it

Add a Matrix entry to `~/.config/tirc/init.lua`:

```lua
config.servers = {
  {
    protocol = 'matrix',
    homeserver = 'https://localhost:8448',
    user_id = '@alice:dendrite.local',
    password = 'alicepassword',
    -- 'auto' (default) probes the homeserver; 'on'/'off' force the driver.
    -- Point at https://localhost:8449 (continuwuity) to exercise sliding sync.
    sliding_sync = 'auto',
    -- Both homeservers' TLS certs are signed by this throwaway dev CA.
    root_ca_pem = io.open('dev/matrix/tls/ca.cert.pem'):read('*a'),
  },
}
```

(`tirc.dev`'s `matrix`/`matrix_sliding` entries embed the CA cert directly, no
filesystem access needed - see `crates/tirc-lua/lua/tirc/dev.lua`.)

Rooms you are already joined to appear as named buffers on startup; messages,
membership and topic changes render through the normalized theme. From a
Matrix-focused buffer, `:list` shows the public room directory and `:j <roomid>`
joins a room.

On the sliding-sync homeserver the buffer list paints as rooms stream in from
the windowed room list, and per-room history loads from each room's timeline
rather than an up-front backfill.

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
rm -rf ~/.local/share/tirc/matrix/_alice_dendrite_local@* ~/.local/share/tirc/matrix/_alice_continuwuity_local@*
```

E2E-encrypted rooms work: the SDK persists its crypto state in the per-account
sqlite store, sends are auto-encrypted, and incoming events are decrypted when
the keys are available. A freshly-logged-in session is unverified, so messages
from senders that only share keys with verified devices show as `[unable to
decrypt ...]` until you verify this session from another client. Interactive
(SAS) verification from within tirc is not implemented yet.

## TLS certs

`tls/` holds a throwaway CA (`ca.key.pem`/`ca.cert.pem`) and CA-signed leaf
certs for each homeserver (`dendrite.*`, `continuwuity.*`), plus Dendrite's
Matrix signing key (`dendrite_signing_key.pem`). All committed - they're
throwaway dev-only material, same as `dev/irc/tls/`. Regenerate with:

```bash
cd dev/matrix/tls
openssl genrsa -out ca.key.pem 2048
openssl req -x509 -new -nodes -key ca.key.pem -sha256 -days 3650 -out ca.cert.pem -subj "/CN=tirc-dev-matrix-ca"

for name_cn in "dendrite dendrite.local" "continuwuity continuwuity.local"; do
  set -- $name_cn
  openssl genrsa -out "$1.key.pem" 2048
  openssl req -new -key "$1.key.pem" -out "$1.csr.pem" -subj "/CN=$2"
  printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:%s,DNS:localhost\n' "$2" > "$1.ext"
  openssl x509 -req -in "$1.csr.pem" -CA ca.cert.pem -CAkey ca.key.pem -CAcreateserial -out "$1.cert.pem" -days 3650 -sha256 -extfile "$1.ext"
  rm -f "$1.csr.pem" "$1.ext"
done
rm -f ca.cert.srl

docker run --rm -v "$PWD:/out" --entrypoint /usr/bin/generate-keys \
  docker.io/matrixdotorg/dendrite-monolith:latest -private-key /out/dendrite_signing_key.pem
```

After regenerating, update the embedded `dev_matrix_ca_pem` string in
`crates/tirc-lua/lua/tirc/dev.lua` to match the new `ca.cert.pem`.
