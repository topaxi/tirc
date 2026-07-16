--- Type stub for the native `tirc.hash` module (implemented in Rust); the
--- real functions are registered by `create_tirc_hash_lua_module`.
---@class TircHashModule
---@field xxh3 fun(s: string): integer deterministic non-negative 53-bit hash (XXH3-64 truncated), stable across runs and platforms
local M = {}

return M
