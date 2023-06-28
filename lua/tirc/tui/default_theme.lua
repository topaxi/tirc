local tirc = require('tirc')
local utils = require('tirc.utils')
local theme = require('tirc.tui.theme')

local M = {}

local blue = theme.style { fg = 'blue' }
local green = theme.style { fg = 'green' }
local darkgray = theme.style { fg = 'darkgray' }

local server_notice_icon = {
  { '-', blue },
  { '!', theme.style { fg = '#ffffff' } },
  { '-', blue },
  ' ',
}

local function format_join(msg)
  return msg.prefix.Nickname[1]
    .. (msg.command.JOIN[3] and msg.command.JOIN[3] ~= 'Unknown' and (' (' .. msg.command.JOIN[3] .. ')') or '')
    .. ' has joined '
    .. msg.command.JOIN[1]
end

local function format_part(msg)
  return msg.prefix.Nickname[1] .. ' has parted ' .. msg.command.PART[1]
end

local function format_privmsg(msg, nickname)
  local is_draft = utils.list_find(msg.tags, function(tag)
    return tag[1] == 'time'
  end) == nil

  if is_draft then
    return {
      { '<', darkgray },
      { nickname, darkgray },
      { '>', darkgray },
      ' ',
      { msg.command.PRIVMSG[2], darkgray },
    }
  end

  return {
    { '<', darkgray },
    { msg.prefix.Nickname[1], blue },
    { '>', darkgray },
    ' ',
    msg.command.PRIVMSG[2],
  }
end

local function format_notice(msg)
  if msg.prefix.ServerName then
    return {
      { '!' .. msg.prefix.ServerName, green },
      ' ',
      msg.command.NOTICE[2],
    }
  elseif msg.prefix.Nickname then
    return {
      '-',
      msg.prefix.Nickname[1],
      '(',
      msg.prefix.Nickname[3],
      ')- ',
      msg.command.NOTICE[2],
    }
  end
end

local function is_string(v)
  return type(v) == 'string'
end

local function is_added_mode(v)
  return type(v) == 'table' and v.Plus
end

local function is_removed_mode(v)
  return type(v) == 'table' and v.Minus
end

local function is_noprefix_mode(v)
  return type(v) == 'table' and v.NoPrefix
end

local get_first_table_key = function(t)
  for k, _ in pairs(t) do
    return k
  end
end

local function format_mode_value(mode_value)
  if is_string(mode_value) then
    return mode_value
  elseif type(mode_value) == 'table' then
    local key = get_first_table_key(mode_value)

    return key .. '(' .. mode_value[key] .. ')'
  end
end

local function format_mode(mode)
  if mode.Plus then
    return '+' .. format_mode_value(mode.Plus[1])
  elseif mode.Minus then
    return '-' .. format_mode_value(mode.Minus[1])
  elseif mode.NoPrefix then
    return format_mode_value(mode.NoPrefix[1])
  end
end

local function format_channel_mode(msg)
  local plus = utils.list_map(
    utils.list_filter(msg.command.ChannelMODE[2], is_added_mode),
    format_mode
  )

  local minus = utils.list_map(
    utils.list_filter(msg.command.ChannelMODE[2], is_removed_mode),
    format_mode
  )

  local noprefix = utils.list_map(
    utils.list_filter(msg.command.ChannelMODE[2], is_noprefix_mode),
    format_mode
  )

  return {
    'cmode/',
    msg.command.ChannelMODE[1],
    #plus > 0 and ' [' .. table.concat(plus, ' ') .. ']' or '',
    #minus > 0 and ' [' .. table.concat(minus, ' ') .. ']' or '',
    #noprefix > 0 and ' [' .. table.concat(noprefix, ' ') .. ']' or '',
  }
end

local function format_user_mode(msg)
  if false then
    return utils.dump_table(msg)
  end

  local plus = utils.list_map(
    utils.list_filter(msg.command.UserMODE[2], is_added_mode),
    format_mode
  )

  local minus = utils.list_map(
    utils.list_filter(msg.command.UserMODE[2], is_removed_mode),
    format_mode
  )

  local noprefix = utils.list_map(
    utils.list_filter(msg.command.UserMODE[2], is_noprefix_mode),
    format_mode
  )

  return {
    'umode/',
    msg.command.UserMODE[1],
    #plus > 0 and ' [' .. table.concat(plus, ' ') .. ']' or '',
    #minus > 0 and ' [' .. table.concat(minus, ' ') .. ']' or '',
    #noprefix > 0 and ' [' .. table.concat(noprefix, ' ') .. ']' or '',
  }
end

local function format_message(msg, nickname)
  if msg.command.JOIN then
    return format_join(msg)
  elseif msg.command.PART then
    return format_part(msg)
  elseif msg.command.PRIVMSG then
    return format_privmsg(msg, nickname)
  elseif msg.command.NOTICE then
    return format_notice(msg)
  elseif msg.command.ChannelMODE then
    return format_channel_mode(msg)
  elseif msg.command.UserMODE then
    return format_user_mode(msg)
  elseif msg.command.Response then
    if
      msg.command.Response[1] == 'RPL_NAMREPLY'
      or msg.command.Response[1] == 'RPL_ENDOFNAMES'
    then
      return nil
    end

    return utils.list_concat(server_notice_icon, {
      table.concat(msg.command.Response[2], ' ', 2),
    })
  elseif msg.command.PING or msg.command.PONG then
    return nil
  elseif msg.command.CAP then
    return utils.list_concat(server_notice_icon, {
      'Capabilities ' .. msg.command.CAP[2] .. ' ' .. msg.command.CAP[3],
    })
    -- elseif msg.command.UserMODE then
    --   local mode = msg.command.UerMode[2].Plus and '+' or '-';

    --   return '-!- Mode change [' .. mode .. ']';
  end

  return tostring(msg)
end

--- @return string|nil
local function get_time_from_tags(tags)
  if not tags or type(tags) ~= 'table' then
    return
  end

  for _, tag in ipairs(tags) do
    if tag[1] == 'time' then
      return tag[2]
    end
  end
end

local function get_time_from_iso_string(str)
  local year, month, day, hour, minute, second =
    str:match('(%d%d%d%d)-(%d%d)-(%d%d)T(%d%d):(%d%d):(%d%d)')
  return {
    year = tonumber(year),
    month = tonumber(month),
    day = tonumber(day),
    hour = tonumber(hour),
    minute = tonumber(minute),
    second = tonumber(second),
  }
end

local function format_iso_time_string(str)
  local time = get_time_from_iso_string(str)
  -- TODO: Adjust time to local time
  return string.format('%02d:%02d:%02d', time.hour, time.minute, time.second)
end

local function format_time(date_time, _msg)
  -- local time_tag = get_time_from_tags(msg.tags)

  -- if time_tag then
  --   return {
  --     format_iso_time_string(time_tag),
  --     { ' ▏', darkgray },
  --   }
  -- end

  return {
    string.format(
      '%02d:%02d:%02d',
      date_time.hour,
      date_time.minute,
      date_time.second
    ),
    { ' ▏', darkgray },
  }
end

function M.setup(config)
  tirc.on('format-time', function(date_time, msg)
    if config.debug then
      return nil
    end

    local ok, str = pcall(format_time, date_time, msg)

    if ok then
      return str
    else
      return 'ERR in format-time: ' .. tostring(str)
    end
  end)

  tirc.on('format-message', function(msg, nickname)
    if config.debug then
      return utils.dump_table(msg)
    end

    local ok, str = pcall(format_message, msg, nickname)

    if ok then
      return str
    else
      return 'ERR in format-message: ' .. tostring(str)
    end
  end)
end

return M
