--- Auto-reply to direct messages while away.
---
--- Usage from `init.lua`:
---
---   tirc.use(require('tirc.plugins.away'), {
---     cooldown = 600,
---     reply_to_mentions = true,
---   })
---
--- `:away [message]` marks you away everywhere (native away where the protocol
--- supports it: IRC AWAY, Matrix presence, Mattermost status); `:away` with no
--- arguments toggles, and the plugin's `:back` command clears. While away,
--- incoming direct messages (and channel mentions, when opted in) get one
--- notice-style auto-reply per sender per cooldown period.
---
--- IRC is excluded from auto-replies by default: servers answer PMs to an away
--- user natively with RPL_AWAY, so a Lua reply would be a duplicate.
---
--- Known limitations: away state is not re-sent to backends on reconnect, and
--- the plugin's Lua-side state is rebuilt on `:reload` from the host's re-emit
--- of the `away` event.

local tirc = require('tirc')
local notify = require('tirc.plugins.notify')

--- Options for the away plugin.
---@class TircAwayOptions
---@field cooldown? integer seconds between auto-replies per sender (default 300)
---@field reply_to_mentions? boolean also auto-reply to channel mentions (default false)
---@field protocols? table<string, boolean> protocols to auto-reply on (default { matrix = true, mattermost = true }; IRC excluded, the server replies natively)
---@field prefix? string auto-reply prefix (default '[away] ')
---@field reply? fun(event: TircEvent, text: string) executor override, replaces the send_notice reply
---@field now? fun(): integer clock override (default os.time)

local M = {}

--- Current away state. `last_reply` maps `backend_id .. ':' .. sender_id` to
--- the time of the last auto-reply, reset whenever the away state changes.
local state = { away = false, message = nil, last_reply = {} }

--- Whether the user is currently away (for user themes/status bars).
---@return boolean
function M.is_away()
  return state.away
end

--- The current away message, or nil when not away.
---@return string|nil
function M.message()
  return state.message
end

--- The auto-reply decision, kept pure for testability: all runtime state
--- arrives via `ctx`.
---@param event TircEvent
---@param ctx { away: boolean, last_reply: table<string, integer>, now: integer, opts: TircAwayOptions }
---@return boolean
function M.should_reply(event, ctx)
  local opts = ctx.opts

  if not ctx.away then
    return false
  end

  if event.type ~= 'message' or event.pending or event.redacted then
    return false
  end

  -- Never auto-reply to notices: they are themselves automated replies, and
  -- answering them risks reply loops between two away clients.
  if event.kind == 'notice' then
    return false
  end

  -- Own echoes never trigger a reply.
  local nick = event.backend.nickname
  if nick ~= '' and event.sender.id:lower() == nick:lower() then
    return false
  end

  if not opts.protocols[event.backend.protocol] then
    return false
  end

  if not notify.is_dm(event) then
    if
      not (opts.reply_to_mentions and notify.is_mention(event.body.text, nick))
    then
      return false
    end
  end

  local key = event.backend.id .. ':' .. event.sender.id
  local last = ctx.last_reply[key]
  return not (last and ctx.now - last < (opts.cooldown or 300))
end

---@param opts? TircAwayOptions
---@return TircAwayOptions
local function normalize(opts)
  opts = opts or {}
  opts.protocols = opts.protocols or { matrix = true, mattermost = true }
  return opts
end

---@param opts? TircAwayOptions
function M:setup(opts)
  opts = normalize(opts)

  tirc.on('away', function(message)
    state.away = message ~= nil
    state.message = message
    -- A fresh away period starts with fresh cooldowns.
    state.last_reply = {}
  end)

  tirc.on('event', function(event, sender)
    local now = (opts.now or os.time)()
    local ctx = {
      away = state.away,
      last_reply = state.last_reply,
      now = now,
      opts = opts,
    }

    if not M.should_reply(event, ctx) then
      return
    end

    state.last_reply[event.backend.id .. ':' .. event.sender.id] = now
    local text = (opts.prefix or '[away] ') .. (state.message or '')
    if opts.reply then
      opts.reply(event, text)
    else
      sender.send_notice(event.target, text)
    end
  end)

  tirc.create_command('back', function()
    -- Routed through the host so native away clears on every backend and the
    -- `away` event fires for all plugins.
    tirc.set_away(nil)
  end, { desc = 'Clear your away status' })
end

return M
