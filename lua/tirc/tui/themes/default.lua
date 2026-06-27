local tirc = require('tirc')
local utils = require('tirc.utils')
local theme = require('tirc.tui.theme')

local M = {}

local white = theme.style { fg = '#ffffff' }
local twhite = theme.style { fg = 'white' } -- this is darker than gray..
local blue = theme.style { fg = 'blue' }
local green = theme.style { fg = 'green' }
local red = theme.style { fg = 'red' }
local gray = theme.style { fg = 'gray' }
local darkgray = theme.style { fg = 'darkgray' }

local server_notice_icon = {
  { { '-', { '!', white }, '-' }, blue },
  ' ',
}

---@param word string
local function is_channel(word)
  return word:match('#%w+$')
end

--- Splits a message body into spans, highlighting channel-like words.
---@param message string
local function format_body(message)
  local spans = utils.list_flat_map(utils.split(message, '%s'), function(word)
    if is_channel(word) then
      return { { word, green }, ' ' }
    end

    return { word, ' ' }
  end)

  table.remove(spans)

  return spans
end

---@param name string
---@param style TircThemeStyle
local function format_nickname(name, style)
  return {
    { '<', gray },
    { name, style },
    { '>', gray },
  }
end

---@param name string
---@param style TircThemeStyle
local function format_action_nickname(name, style)
  return { { '* ', name }, style }
end

--- A normal or action message. Pending (optimistic, unconfirmed) messages dim.
---@param event TircEvent
local function format_message(event)
  local name = event.sender.name
  local text = event.body.text
  local is_action = event.kind == 'action'

  if event.pending then
    return {
      is_action and format_action_nickname(name, darkgray)
        or format_nickname(name, darkgray),
      ' ',
      { format_body(text), darkgray },
    }
  end

  if event.kind == 'notice' then
    return {
      { '-', gray },
      { name, blue },
      { '- ', gray },
      format_body(text),
    }
  end

  return {
    is_action and format_action_nickname(name, white)
      or format_nickname(name, blue),
    ' ',
    format_body(text),
  }
end

---@param event TircEvent
local function format_membership(event)
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

  local line = { { event.who.name, blue } }

  -- Extended-join real name, e.g. `topaxi (Damian) has joined #tirc`.
  if change == 'join' and event.realname and event.realname ~= 'Unknown' then
    line[#line + 1] = {
      { ' (', gray },
      { event.realname, blue },
      { ')', gray },
    }
  end

  line[#line + 1] = { verb, twhite }
  line[#line + 1] = { event.target_name or event.target, green }

  if event.reason and event.reason ~= '' then
    line[#line + 1] = { ' (' .. event.reason .. ')', gray }
  end

  return line
end

---@param event TircEvent
local function format_topic(event)
  local who = event.who and event.who.name or nil

  return {
    who and { { who, blue }, { ' changed the topic to ', twhite } }
      or { 'Topic: ', twhite },
    { event.topic, green },
  }
end

---@param event TircEvent
local function format_rename(event)
  return {
    { event.who.name, blue },
    { ' is now known as ', twhite },
    { event.new, blue },
  }
end

---@param event TircEvent
local function format_quit(event)
  local line = {
    { event.who.name, blue },
    { ' has quit', twhite },
  }

  if event.reason and event.reason ~= '' then
    line[#line + 1] = { ' (' .. event.reason .. ')', gray }
  end

  return line
end

---@param modestring string
local function format_modestring(modestring)
  local spans = {}

  for ch in modestring:gmatch('.') do
    if ch == '+' then
      spans[#spans + 1] = { ch, green }
    elseif ch == '-' then
      spans[#spans + 1] = { ch, red }
    else
      spans[#spans + 1] = ch
    end
  end

  return spans
end

--- Renders a MODE line from `event.text` of `<target> <modestring> [args]`,
--- e.g. `cmode/#tirc +nt` or `umode/topaxi +iwxz`.
---@param event TircEvent
local function format_mode(event)
  local parts = utils.split(event.text, '%s')
  local target = parts[1] or ''
  local modestring = parts[2] or ''
  local is_channel_mode = target:match('^[#&]') ~= nil
  local prefix = is_channel_mode and 'cmode' or 'umode'

  local result = {
    { prefix .. '/', twhite },
    { target, is_channel_mode and green or blue },
    ' ',
    format_modestring(modestring),
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
local function format_server_info(event)
  if event.code == 'MODE' then
    return format_mode(event)
  end

  -- Server notices keep the originating server name, like `!irc.example.com ...`.
  if event.code == 'NOTICE' and event.from then
    return {
      { '!' .. event.from, green },
      ' ',
      event.text,
    }
  end

  return utils.list_concat(server_notice_icon, { event.text })
end

local role_prefix = {
  owner = { '~', red },
  admin = { '&', red },
  op = { '@', red },
  halfop = { '%', red },
  voice = { '+', green },
  member = {},
}

---@type TircUi
M.ui = {
  format = {
    buffer_title = function(server, nickname, buffer_name)
      return {
        { nickname, blue },
        { '@', twhite },
        { server, green },
        { ' in ', twhite },
        { buffer_name, green },
      }
    end,

    message_time = function(dt, _event)
      local is_1337 = dt.hour == 13 and dt.minute == 37

      return {
        {
          string.format('%02d:%02d:%02d', dt.hour, dt.minute, dt.second),
          is_1337 and red or twhite,
        },
        { ' ▏', twhite },
      }
    end,

    ---@param event TircEvent
    message_text = function(event, _nickname)
      local kind = event.type

      if event.redacted then
        return { { '[message deleted]', darkgray } }
      end

      if kind == 'message' then
        return format_message(event)
      elseif kind == 'membership' then
        return format_membership(event)
      elseif kind == 'topic' then
        return format_topic(event)
      elseif kind == 'rename' then
        return format_rename(event)
      elseif kind == 'quit' then
        return format_quit(event)
      elseif kind == 'server_info' then
        return format_server_info(event)
      elseif kind == 'edit' then
        return { format_body(event.body.text), { ' (edited)', darkgray } }
      end

      return nil
    end,

    ---@param user TircUser
    user = function(user)
      return {
        role_prefix[user.role] or {},
        { user.name, blue },
      }
    end,
  },
}

---@class (exact) TircThemeDefaultOptions

---@param _config TircThemeDefaultOptions
function M.setup(_config)
  tirc.ui = M.ui
end

return M
