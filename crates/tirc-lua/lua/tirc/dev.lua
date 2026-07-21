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

--- Conduit, the classic-`/sync` homeserver (see dev/matrix/docker-compose.yml).
---@type TircMatrixServer
local matrix = {
  protocol = 'matrix',
  homeserver = 'http://localhost:6167',
  user_id = '@alice:localhost',
  password = 'alicepassword',
  sliding_sync = 'off',
  metadata = { label = 'dev-matrix' },
}

--- continuwuity, the Simplified Sliding Sync (MSC4186) homeserver.
---@type TircMatrixServer
local matrix_sliding = {
  protocol = 'matrix',
  homeserver = 'http://localhost:6168',
  user_id = '@alice:localhost',
  password = 'alicepassword',
  sliding_sync = 'on',
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
