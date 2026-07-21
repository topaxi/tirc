--- Server entries for the throwaway local servers under `dev/` (see
--- `dev/irc/README.md`, `dev/matrix/README.md`, `dev/mattermost/README.md`).
--- Not useful outside this repo's checkout - only require it from a
--- development `init.lua`, e.g.:
---
---   local dev = require('tirc.dev')
---   local utils = require('tirc.utils')
---   config.servers = utils.list_concat(dev.servers, my_real_servers)
---
--- Or pick individual entries (`dev.irc`, `dev.matrix`, `dev.matrix_sliding`,
--- `dev.mattermost`) to combine only some of the dev servers with your own.
---
--- This module only exists in dev builds (`cargo run`, `cargo test`). Release
--- builds register a native replacement in Rust that exposes nothing but
--- `is_dev()`, returning `false` - none of the servers below are compiled into
--- a release binary, not even as an embedded string. Guard usage with
--- `is_dev()` if your init.lua is shared between a dev checkout and a release
--- install.

---@type TircIrcServer
local irc = {
  protocol = 'irc',
  host = 'localhost',
  port = 6667,
  use_tls = false,
  nickname = { 'alice' },
  autojoin = { '#test' },
  metadata = { label = 'dev-irc' },
}

--- Throwaway CA (see dev/matrix/tls/) both dev homeservers' TLS certs are
--- signed by, needed because Matrix federation is always HTTPS. The two
--- homeservers federate with each other; see dev/matrix/README.md.
local dev_matrix_ca_pem = [[
-----BEGIN CERTIFICATE-----
MIIDGzCCAgOgAwIBAgIUD8wcWUuQX+DphFFXDq2NwTf/f4QwDQYJKoZIhvcNAQEL
BQAwHTEbMBkGA1UEAwwSdGlyYy1kZXYtbWF0cml4LWNhMB4XDTI2MDcyMTA5NDQ0
MloXDTM2MDcxODA5NDQ0MlowHTEbMBkGA1UEAwwSdGlyYy1kZXYtbWF0cml4LWNh
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAs4a71vDBH4bcxQxA1gQh
wlb0bOA7Fsiy4iMSUoeNv9tEAh4QjPRN8k6n5k1N5o8vUkt8GvaH6injQiKCV5tj
GV8n5ROyt6IkAoBv5VC5PyofU5akaGRXaB24UbijxXNyXtyXGtUQrYtzi/2+B9Gl
MzfvGKKXttecobLgICgKDuX0NdIb4TIMHQIgRztGN1zmcVOfjzk0UriPYoelAx4f
ayj96Z8ko/jJw1RTsfs50C/Q8DHg6jnykwneEBREO3m48RMQNQXW1MufzXXF70Kb
Th0TmtofvnudbyJbBWq3C5aozDY3X53DQX+cKPGWfdb4EbWH/c9EQCjOL0CE+U8p
gwIDAQABo1MwUTAdBgNVHQ4EFgQUWVURIW3A/+wxpi64rxP5G/1Jd6YwHwYDVR0j
BBgwFoAUWVURIW3A/+wxpi64rxP5G/1Jd6YwDwYDVR0TAQH/BAUwAwEB/zANBgkq
hkiG9w0BAQsFAAOCAQEAbG4ajFwT8v+nTO3pstBqe2veEbkbUmpLLyW3+voEORzc
pmgQ+Vqpopgqap6RrDUz2U/zb2yyK9u47h5Kq3NRx5Ue2fkK7XDim7xJA9srwOlk
hPlPBWjBGs2+eixE1zQzcAeHBgz8Q4C+Dux4zs39uM7wgc7GlNVu+CBl/Sjof82Y
wFhelnOLG89cKLDBLqMsRF7FaydtVOsOIQ8ay3OYNVN187ifF30ruX00rdQ0cSyl
+HpFHifmmoNAvyN5tv9ATlfrJlJIeOX/2fnJ60raE47aWpRmeRtuHjMEdk8xUyLf
4dKUG4cinXeISrHCfIncN4DnVr8zSSCg4EEgWq7GWg==
-----END CERTIFICATE-----
]]

--- Rooms set up by dev/matrix/setup-rooms.sh once both alice accounts exist
--- (see dev/matrix/README.md): one local room per homeserver, plus one
--- shared room federated between both.
local dev_matrix_shared_room = '#tirc-dev:continuwuity.local'

--- Dendrite, the classic-`/sync` homeserver (see dev/matrix/docker-compose.yml).
---@type TircMatrixServer
local matrix = {
  protocol = 'matrix',
  homeserver = 'https://localhost:8448',
  user_id = '@alice:dendrite.local',
  password = 'alicepassword',
  sliding_sync = 'off',
  root_ca_pem = dev_matrix_ca_pem,
  autojoin = { '#tirc-local:dendrite.local', dev_matrix_shared_room },
  metadata = { label = 'dev-matrix' },
}

--- continuwuity, the Simplified Sliding Sync (MSC4186) homeserver.
---@type TircMatrixServer
local matrix_sliding = {
  protocol = 'matrix',
  homeserver = 'https://localhost:8449',
  user_id = '@alice:continuwuity.local',
  password = 'alicepassword',
  sliding_sync = 'on',
  root_ca_pem = dev_matrix_ca_pem,
  autojoin = { '#tirc-local:continuwuity.local', dev_matrix_shared_room },
  metadata = { label = 'dev-matrix-sliding' },
}

---@type TircMattermostServer
local mattermost = {
  protocol = 'mattermost',
  url = 'http://localhost:8065',
  user_id = 'alice',
  password = 'alicepassword1!',
  team = 'testteam',
  autojoin = { 'town-square' },
  metadata = { label = 'dev-mattermost' },
}

local M = {
  irc = irc,
  matrix = matrix,
  matrix_sliding = matrix_sliding,
  mattermost = mattermost,
}

---@type TircConfigServer[]
M.servers = { irc, matrix, matrix_sliding, mattermost }

--- Always `true` here (the dev build). The release build's native stand-in
--- for this module returns `false` instead.
---@return boolean
function M.is_dev()
  return true
end

return M
