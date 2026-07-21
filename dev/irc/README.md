# Local IRC server for development

A throwaway [UnrealIRCd](https://www.unrealircd.org/) 6 server for testing the IRC
backend. Open to anyone, no NickServ/services, self-signed TLS cert bundled in-repo.
Never expose this; it has no real security.

## Start

```bash
docker compose -f dev/irc/docker-compose.yml up -d
```

Give it a few seconds to boot (it verifies its config and generates a couple of local
databases on first start). Check readiness with:

```bash
docker compose -f dev/irc/docker-compose.yml logs -f
```

## Point tirc at it

Add an IRC entry to `~/.config/tirc/init.lua`. Either the plaintext listener:

```lua
config.servers = {
  {
    protocol = 'irc',
    host = 'localhost',
    port = 6667,
    use_tls = false,
    nickname = { 'alice' },
    autojoin = { '#test' },
  },
}
```

or the TLS listener, using the bundled throwaway self-signed cert (`dev/irc/tls/`):

```lua
config.servers = {
  {
    protocol = 'irc',
    host = 'localhost',
    port = 6697,
    use_tls = true,
    accept_invalid_cert = true,
    nickname = { 'alice' },
    autojoin = { '#test' },
  },
}
```

No registration step is needed - just connect with any nickname. Channels are created
on `JOIN`.

## IRCOps

No `oper` block is configured. If you need operator privileges, hash a password inside
the running container and add an `oper` block to `dev/irc/unrealircd.conf` yourself:

```bash
docker compose -f dev/irc/docker-compose.yml exec unrealircd \
  /app/unrealircd/bin/unrealircd mkpasswd
```

See the [Oper block](https://www.unrealircd.org/docs/Oper_block) docs, then
`docker compose -f dev/irc/docker-compose.yml restart` to apply.

## Stop / reset

```bash
docker compose -f dev/irc/docker-compose.yml down        # stop
```

State (channel/reputation/TKL databases) lives inside the container's writable layers,
not a named volume, so `down` without `-v` still resets it on next recreate.

## chaosircd

The task that added this directory also asked for
[chaosircd](https://github.com/rsenn/chaosircd). It does not build with a modern
compiler: for `__GNUC__ > 4` its `defs.h` disables the `CHAOS_INLINE_FN` macro (marked
with `#warning GNUC > 4`), which drops inline function bodies and leaves declarations
without terminating semicolons, cascading into parse errors throughout the codebase.
Reproduced on both glibc (Debian) and musl (Alpine) with cmake/gcc 12 - this is an
upstream bitrot issue, not a container/toolchain quirk, and fixing it would mean
patching the vendored `libchaos`/`libowfat` sources rather than just packaging them.
Left out for now; ask if you'd like a patch attempt or a different second IRC server
(e.g. InspIRCd or solanum, both of which have straightforward Docker builds).
