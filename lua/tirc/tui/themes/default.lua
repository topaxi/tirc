local tirc = require('tirc')
local utils = require('tirc.utils')
local theme = require('tirc.tui.theme')
local Class = require('tirc.class')

--- The bundled default theme, structured as a class so downstream themes can
--- reuse and extend it. It is a superset of `TircUi`: `new()` returns an instance
--- whose formatter methods satisfy the contract directly on the object.
---
--- Extend by subclassing and overriding methods:
--- ```lua
--- local Default = require('tirc.tui.themes.default')
--- local My = Default.extend()
--- function My:format_message(event)
---   return Default.format_message(self, event) -- call up to the base
--- end
--- tirc.ui = My.new()
--- ```
---
--- ...or compose via constructor options without subclassing:
--- ```lua
--- tirc.ui = Default.new {
---   palette = { blue = theme.style { fg = 'cyan' } },
---   buffer_title = function(self, server, nickname, buffer_name) ... end,
--- }
--- ```
---
--- `render_buffer_bar` owns the whole bar layout and returns `{ rows = { ... } }`
--- (one row per line). Override it to group tabs onto separate rows per backend:
--- ```lua
--- function My:render_buffer_bar(buffers)
---   local groups, order = {}, {}
---   for _, b in ipairs(buffers) do
---     if not groups[b.backend_id] then
---       groups[b.backend_id] = {}
---       order[#order + 1] = b.backend_id
---     end
---     local g = groups[b.backend_id]
---     g[#g + 1] = self:render_buffer_tab(b)
---   end
---   local rows = {}
---   for _, id in ipairs(order) do
---     rows[#rows + 1] = groups[id]
---   end
---   return { rows = rows }
--- end
--- ```
--- Options accepted by `TircTheme.new`/`setup`.
--- Formatter overrides are stored on the instance and called method-style, so
--- each receives `self` as its first parameter, exactly like the class methods
--- they replace.
---@class TircThemeOptions
---@field palette? table<string, TircThemeStyle> override individual colours
---@field buffer_title? fun(self: TircTheme, server: string, nickname: string, buffer: string): TircSpans
---@field userlist_title? fun(self: TircTheme, buffer: string): TircSpans
---@field message_time? fun(self: TircTheme, date_time: TircDateTime, event: TircEvent): TircSpans
---@field message_text? fun(self: TircTheme, event: TircEvent, nickname: string): TircSpans?
---@field user? fun(self: TircTheme, user: TircUser): TircSpans
---@field render_buffer_tab? fun(self: TircTheme, buffer: TircBufferTab): TircSpans
---@field render_buffer_bar? fun(self: TircTheme, buffers: TircBufferTab[]): TircBufferBar | TircSpans

---@class TircTheme: TircUi, TircClassDef<TircTheme, TircThemeOptions>
---@field styles table<string, TircThemeStyle>
---@field setup fun(self: TircTheme, opts?: TircThemeOptions) plugin entry point for `tirc.use`
local Theme = Class.new()

--- Initialises an instance: builds the palette and applies any formatter
--- overrides passed as options. Overrides are stored as plain instance fields and
--- called method-style, so they receive `self` like a regular method.
---@param opts? TircThemeOptions
function Theme:init(opts)
  opts = opts or {}

  self.styles = self:make_styles(opts.palette)

  for key, value in pairs(opts) do
    if key ~= 'palette' then
      self[key] = value
    end
  end
end

--- Plugin entry point used by `tirc.use(theme)`. Called method-style, so `self`
--- is the theme class: a subclass (`Theme.extend()`) instantiates itself and its
--- formatter overrides take effect.
---@param opts? TircThemeOptions
function Theme:setup(opts)
  require('tirc').ui = self.new(opts)
end

--- The colour palette. Override this method (or pass `opts.palette`) to re-theme.
---@param overrides? table<string, TircThemeStyle>
function Theme:make_styles(overrides)
  local styles = {
    white = theme.style { fg = '#ffffff' },
    twhite = theme.style { fg = 'white' }, -- this is darker than gray..
    blue = theme.style { fg = 'blue' },
    green = theme.style { fg = 'green' },
    red = theme.style { fg = 'red' },
    gray = theme.style { fg = 'gray' },
    darkgray = theme.style { fg = 'darkgray' },
  }

  if overrides then
    for name, style in pairs(overrides) do
      styles[name] = style
    end
  end

  return styles
end

--- The `-!-` server-notice icon.
function Theme:server_notice_icon()
  local s = self.styles
  return {
    { { '-', { '!', s.white }, '-' }, s.blue },
    ' ',
  }
end

--- Maps a member role to its prefix span.
function Theme:role_styles()
  local s = self.styles
  return {
    owner = { '~', s.red },
    admin = { '&', s.red },
    op = { '@', s.red },
    halfop = { '%', s.red },
    voice = { '+', s.green },
    member = {},
  }
end

---@param word string
function Theme:is_channel(word)
  return word:match('#%w+$')
end

--- Splits a message body into spans, highlighting channel-like words.
---@param message string
function Theme:format_body(message)
  local green = self.styles.green
  local spans = utils.list_flat_map(utils.split(message, '%s'), function(word)
    if self:is_channel(word) then
      return { { word, green }, ' ' }
    end

    return { word, ' ' }
  end)

  table.remove(spans)

  return spans
end

---@param name string
---@param style TircThemeStyle
function Theme:format_nickname(name, style)
  local gray = self.styles.gray
  return {
    { '<', gray },
    { name, style },
    { '>', gray },
  }
end

---@param name string
---@param style TircThemeStyle
function Theme:format_action_nickname(name, style)
  return { { '* ', name }, style }
end

--- A normal or action message. Pending (optimistic, unconfirmed) messages dim.
---@param event TircEvent
function Theme:format_message(event)
  local s = self.styles
  local name = event.sender.name
  local text = event.body.text
  local is_action = event.kind == 'action'

  if event.pending then
    return {
      is_action and self:format_action_nickname(name, s.darkgray)
        or self:format_nickname(name, s.darkgray),
      ' ',
      { self:format_body(text), s.darkgray },
    }
  end

  if event.kind == 'notice' then
    return {
      { '-', s.gray },
      { name, s.blue },
      { '- ', s.gray },
      self:format_body(text),
    }
  end

  return {
    is_action and self:format_action_nickname(name, s.white)
      or self:format_nickname(name, s.blue),
    ' ',
    self:format_body(text),
  }
end

---@param event TircEvent
function Theme:format_membership(event)
  local s = self.styles
  local change = event.change

  -- Roster seeding and role changes do not render a line.
  if change == 'present' or change == 'set_role' then
    return nil
  end

  local verb = ({
    join = ' has joined ',
    part = ' has parted ',
    kick = ' was kicked from ',
    invite = ' was invited to ',
  })[change] or ' '

  local line = { { event.who.name, s.blue } }

  -- Extended-join real name, e.g. `topaxi (Damian) has joined #tirc`.
  if change == 'join' and event.realname and event.realname ~= 'Unknown' then
    line[#line + 1] = {
      { ' (', s.gray },
      { event.realname, s.blue },
      { ')', s.gray },
    }
  end

  line[#line + 1] = { verb, s.twhite }
  line[#line + 1] = { event.target_name or event.target, s.green }

  if event.reason and event.reason ~= '' then
    line[#line + 1] = { ' (' .. event.reason .. ')', s.gray }
  end

  return line
end

---@param event TircEvent
function Theme:format_topic(event)
  local s = self.styles
  local who = event.who and event.who.name or nil

  return {
    who and { { who, s.blue }, { ' changed the topic to ', s.twhite } }
      or { 'Topic: ', s.twhite },
    { event.topic, s.green },
  }
end

---@param event TircEvent
function Theme:format_rename(event)
  local s = self.styles
  return {
    { event.who.name, s.blue },
    { ' is now known as ', s.twhite },
    { event.new, s.blue },
  }
end

---@param event TircEvent
function Theme:format_quit(event)
  local s = self.styles
  local line = {
    { event.who.name, s.blue },
    { ' has quit', s.twhite },
  }

  if event.reason and event.reason ~= '' then
    line[#line + 1] = { ' (' .. event.reason .. ')', s.gray }
  end

  return line
end

---@param modestring string
function Theme:format_modestring(modestring)
  local s = self.styles
  local spans = {}

  for ch in modestring:gmatch('.') do
    if ch == '+' then
      spans[#spans + 1] = { ch, s.green }
    elseif ch == '-' then
      spans[#spans + 1] = { ch, s.red }
    else
      spans[#spans + 1] = ch
    end
  end

  return spans
end

--- Renders a MODE line from `event.text` of `<target> <modestring> [args]`,
--- e.g. `cmode/#tirc +nt` or `umode/topaxi +iwxz`.
---@param event TircEvent
function Theme:format_mode(event)
  local s = self.styles
  local parts = utils.split(event.text, '%s')
  local target = parts[1] or ''
  local modestring = parts[2] or ''
  local is_channel_mode = target:match('^[#&]') ~= nil
  local prefix = is_channel_mode and 'cmode' or 'umode'

  local result = {
    { prefix .. '/', s.twhite },
    { target, is_channel_mode and s.green or s.blue },
    ' ',
    self:format_modestring(modestring),
  }

  if #parts > 2 then
    local args = {}
    for i = 3, #parts do
      args[#args + 1] = parts[i]
    end
    result[#result + 1] = ' '
    result[#result + 1] = table.concat(args, ' ')
  end

  return result
end

---@param event TircEvent
function Theme:format_server_info(event)
  local s = self.styles

  if event.code == 'MODE' then
    return self:format_mode(event)
  end

  -- Server notices keep the originating server name, like `!irc.example.com ...`.
  if event.code == 'NOTICE' and event.from then
    return {
      { '!' .. event.from, s.green },
      ' ',
      event.text,
    }
  end

  return utils.list_concat(self:server_notice_icon(), { event.text })
end

---@param server string
---@param nickname string
---@param buffer_name string
function Theme:buffer_title(server, nickname, buffer_name)
  local s = self.styles
  return {
    { nickname, s.blue },
    { '@', s.twhite },
    { server, s.green },
    { ' in ', s.twhite },
    { buffer_name, s.green },
  }
end

---@param buffer_name string
function Theme:userlist_title(buffer_name)
  return { buffer_name, self.styles.green }
end

---@param dt TircDateTime
---@param _event TircEvent
function Theme:message_time(dt, _event)
  local s = self.styles
  local is_1337 = dt.hour == 13 and dt.minute == 37

  return {
    {
      string.format('%02d:%02d:%02d', dt.hour, dt.minute, dt.second),
      is_1337 and s.red or s.twhite,
    },
    { ' ▏', s.twhite },
  }
end

---@param event TircEvent
---@param _nickname string
function Theme:message_text(event, _nickname)
  local kind = event.type

  if event.redacted then
    return { { '[message deleted]', self.styles.darkgray } }
  end

  if kind == 'message' then
    return self:format_message(event)
  elseif kind == 'membership' then
    return self:format_membership(event)
  elseif kind == 'topic' then
    return self:format_topic(event)
  elseif kind == 'rename' then
    return self:format_rename(event)
  elseif kind == 'quit' then
    return self:format_quit(event)
  elseif kind == 'server_info' then
    return self:format_server_info(event)
  elseif kind == 'edit' then
    return {
      self:format_body(event.body.text),
      { ' (edited)', self.styles.darkgray },
    }
  end

  return nil
end

---@param user TircUser
function Theme:user(user)
  return {
    self:role_styles()[user.role] or {},
    { user.name, self.styles.blue },
  }
end

--- Renders one tab in the buffer bar. Focused tabs are styled brighter.
---@param buffer TircBufferTab
function Theme:render_buffer_tab(buffer)
  local s = self.styles
  local meta = buffer.backend_metadata
  local backend_label = (meta and meta.label) or buffer.backend_name
  local name = tirc.multi_backend and (backend_label .. '/' .. buffer.name)
    or buffer.name

  return { { name, tirc.is_focused_buffer(buffer) and s.white or s.gray }, ' ' }
end

--- Lays out the whole buffer bar. Returns `{ rows = { <TircSpans>, ... } }`, one
--- entry per rendered line. The default is a single row of tabs; override this to
--- group by `buffer.backend_id`, render multiple rows, or filter the buffers.
---@param buffers TircBufferTab[]
---@return TircBufferBar
function Theme:render_buffer_bar(buffers)
  local row = {}

  for _, buffer in ipairs(buffers) do
    row[#row + 1] = self:render_buffer_tab(buffer)
  end

  return { rows = { row } }
end

return Theme
