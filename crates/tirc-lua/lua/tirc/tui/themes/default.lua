local tirc = require('tirc')
local utils = require('tirc.utils')
local theme = require('tirc.tui.theme')
local Class = require('tirc.class')
local BarRow = require('tirc.tui.bar_row')

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
---   buffer_title = function(self, nickname, buffer) ... end,
--- }
--- ```
---
--- `render_buffer_bar` owns the whole bar layout and returns a `TircBufferBar`
--- (one row per line, plus the parallel `ids` click declaration). Build rows
--- with `tirc.tui.bar_row`, which keeps each row's spans and ids in lockstep.
--- Override it to group tabs onto separate rows per backend:
--- ```lua
--- local BarRow = require('tirc.tui.bar_row')
--- function My:render_buffer_bar(buffers)
---   local rows, order = {}, {}
---   for _, b in ipairs(buffers) do
---     local row = rows[b.backend_id]
---     if not row then
---       row = BarRow.new()
---       rows[b.backend_id] = row
---       order[#order + 1] = row
---     end
---     row:add(self:render_buffer_tab(b, row:first()), b.id)
---   end
---   return BarRow.bar(unpack(order))
--- end
--- ```
--- Options accepted by `TircTheme.new`/`setup`.
--- Formatter overrides are stored on the instance and called method-style, so
--- each receives `self` as its first parameter, exactly like the class methods
--- they replace.
---@class TircThemeOptions
---@field palette? table<string, TircThemeStyle> override individual colours
---@field buffer_title? fun(self: TircTheme, nickname: string, buffer: TircBufferTab): TircSpans
---@field userlist_title? fun(self: TircTheme, buffer: string): TircSpans
---@field message_time? fun(self: TircTheme, date_time: TircDateTime, event: TircEvent): TircSpans
---@field message_text? fun(self: TircTheme, event: TircEvent, nickname: string): TircSpans?
---@field link_preview? fun(self: TircTheme, preview: TircLinkPreview): TircSpans[]
---@field render_reactions? fun(self: TircTheme, event: TircEvent, hovered_key: string|nil): TircReactionPill[]
---@field render_quick_reactions? fun(self: TircTheme, event: TircEvent, emojis: string[], hovered_key: string|nil): TircReactionPill[]
---@field user? fun(self: TircTheme, user: TircUser): TircSpans
---@field render_buffer_tab? fun(self: TircTheme, buffer: TircBufferTab): TircSpans
---@field render_buffer_bar? fun(self: TircTheme, buffers: TircBufferTab[]): TircBufferBar | TircSpans
---@field render_unread_separator? fun(self: TircTheme): TircSpans
---@field render_date_separator? fun(self: TircTheme, date: TircDateTime): TircSpans
---@field buffer_bar? 'linear'|'grouped'|'per-backend'|'tabbed'|string buffer-bar layout (default 'linear'); the runtime `:barstyle` override wins. Custom themes may define their own names - unknown names render as 'linear' here
---@field tabbed_click? 'focus'|'select' what clicking a backend tab does in the 'tabbed' layout: 'focus' jumps to that backend's last-viewed buffer, 'select' only switches the visible buffer row (default 'focus')
---@field on_bar_click? fun(self: TircTheme, id: string) handler for clicks on `'custom:<...>'` bar elements; receives the id verbatim. Mutate instance state and/or call `tirc.focus_buffer`/`tirc.select_backend` to drive the UI

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
    yellow = theme.style { fg = 'yellow' },
    gray = theme.style { fg = 'gray' },
    darkgray = theme.style { fg = 'darkgray' },
    tab = theme.style { fg = 'gray', bg = 'darkgray' },
    tab_focused = theme.style { fg = 'white', bg = 'darkgray' },
    tab_unread = theme.style { fg = 'white', bg = 'darkgray' },
    tab_mention = theme.style { fg = 'red', bg = 'darkgray' },
    tab_hover = theme.style { fg = 'white', bg = 'gray' },
    backend_tab = theme.style { fg = 'gray', bg = 'darkgray' },
    backend_tab_selected = theme.style { fg = 'white', bg = 'blue' },
    bar_group_label = theme.style { fg = 'darkgray' },
    unread_separator = theme.style { fg = 'darkgray' },
    reaction = theme.style { fg = 'gray', bg = 'darkgray' },
    reaction_mine = theme.style { fg = 'white', bg = 'blue' },
    reaction_hover = theme.style { fg = 'white', bg = 'gray' },
    -- Underline rather than a background fill: a bright bg would clash with
    -- per-nick foreground colors (e.g. from the nick_colors plugin).
    user_hover = theme.style { underline = true },
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

--- Per-nick style: a registered `tirc.nick_style` provider (e.g. the
--- `tirc.plugins.nick_colors` plugin) wins, else `fallback`. Override to opt a
--- theme out of provider-driven nick colors.
---@param user TircUserRef|TircUser|nil
---@param fallback TircThemeStyle
---@return TircThemeStyle
function Theme:nick_style(user, fallback)
  return user and tirc.nick_style(user) or fallback
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
      { name, self:nick_style(event.sender, s.blue) },
      { '- ', s.gray },
      self:format_body(text),
    }
  end

  return {
    is_action and self:format_action_nickname(
      name,
      self:nick_style(event.sender, s.white)
    ) or self:format_nickname(name, self:nick_style(event.sender, s.blue)),
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

  local line = { { event.who.name, self:nick_style(event.who, s.blue) } }

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
    who and {
      { who, self:nick_style(event.who, s.blue) },
      { ' changed the topic to ', s.twhite },
    } or { 'Topic: ', s.twhite },
    { event.topic, s.green },
  }
end

---@param event TircEvent
function Theme:format_rename(event)
  local s = self.styles
  -- The new nick is a bare string; color it via a pseudo ref so it matches
  -- the user's future messages (for IRC the id is the nick itself).
  return {
    { event.who.name, self:nick_style(event.who, s.blue) },
    { ' is now known as ', s.twhite },
    {
      event.new,
      self:nick_style({ id = event.new, name = event.new }, s.blue),
    },
  }
end

---@param event TircEvent
function Theme:format_quit(event)
  local s = self.styles
  local line = {
    { event.who.name, self:nick_style(event.who, s.blue) },
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

  -- The synthetic `internal` backend carries captured log lines: `event.code`
  -- holds the log level and `event.from` the log target. Color the message by
  -- severity and dim the target prefix.
  if event.backend.protocol == 'internal' then
    local level_style = ({
      ERROR = s.red,
      WARN = s.yellow,
      DEBUG = s.darkgray,
      TRACE = s.darkgray,
    })[event.code]
    local spans = {}
    if event.from and event.from ~= '' then
      spans[#spans + 1] = { event.from .. ' ', s.darkgray }
    end
    spans[#spans + 1] = level_style and { event.text, level_style }
      or event.text
    return spans
  end

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

--- The room header, shown as `nick@server in <room>`. The `server` is the user's
--- own home server when known (a Matrix mxid's domain, e.g. `continuwuity.local`)
--- rather than the raw connection address; it falls back to the backend name for
--- protocols without one (IRC). For federated rooms - those whose target carries a
--- different homeserver (Matrix room ids look like `!id:server`) - that homeserver
--- is appended so `tirc-dev` reads as `tirc-dev:server`; a room on the user's own
--- home server is shown by its plain name. When the buffer has a topic it trails
--- the header like a traditional IRC client.
---@param nickname string
---@param buffer TircBufferTab
function Theme:buffer_title(nickname, buffer)
  local s = self.styles
  local home_server = buffer.home_server

  -- Everything after the first `:` in the target is the room's homeserver
  -- (kept verbatim, so a `host:port` server survives). IRC targets (`#channel`)
  -- have no colon, so this is nil and the plain name is used. The server is
  -- omitted when the room lives on the user's own home server.
  local room_server = buffer.target:match(':(.+)$')
  local room_label = buffer.name
  if room_server and room_server ~= home_server then
    room_label = buffer.name .. ':' .. room_server
  end

  local spans = {
    { nickname, s.blue },
    { '@', s.twhite },
    { home_server or buffer.backend_name, s.green },
    { ' in ', s.twhite },
    { room_label, s.green },
  }

  if buffer.topic and buffer.topic ~= '' then
    spans[#spans + 1] = { '  ', s.twhite }
    spans[#spans + 1] = { utils.truncate(buffer.topic, 120), s.gray }
  end

  return spans
end

---@param buffer_name string
function Theme:userlist_title(buffer_name)
  return { buffer_name, self.styles.green }
end

--- Renders a fetched Open Graph link preview as up to two indented rows: a title
--- row (optional site name, then title) and an optional description row. Returns
--- an array of rows, one TircSpans per row. Return an empty table to hide a
--- preview.
---@param preview TircLinkPreview
---@return TircSpans[]
function Theme:link_preview(preview)
  local s = self.styles
  local rows = {}

  if preview.title and preview.title ~= '' then
    local title_row = { { '\u{258e} ', s.darkgray } }
    if preview.site_name and preview.site_name ~= '' then
      title_row[#title_row + 1] = { preview.site_name .. '  ', s.green }
    end
    title_row[#title_row + 1] = { preview.title, s.white }
    rows[#rows + 1] = title_row
  end

  if preview.description and preview.description ~= '' then
    local description = utils.truncate(preview.description, 200)
    rows[#rows + 1] = { { '\u{258e} ', s.darkgray }, { description, s.gray } }
  end

  return rows
end

---@param dt TircDateTime
---@param _event TircEvent
function Theme:message_time(dt, _event)
  local s = self.styles
  local is_1337 = dt.hour == 13 and dt.minute == 37

  local time = nil

  if is_1337 then
    time = {
      { dt:format('%H:%M'), s.red },
      { dt:format(':%S'), s.twhite },
    }
  else
    time = { dt:format('%H:%M:%S'), s.twhite }
  end

  return {
    time,
    { ' ▏', s.twhite },
  }
end

--- Appends non-image media attachments to a spans table in place, each as a
--- `[kind: name] url` fallback. Image attachments are handled by the renderer
--- (drawn inline when the terminal supports graphics, or shown as their own
--- fallback line otherwise), so they are skipped here to avoid a duplicate label
--- next to the inline picture.
---@param spans table
---@param event TircEvent
function Theme:append_attachments(spans, event)
  local attachments = event.body and event.body.attachments
  if not attachments then
    return
  end
  local s = self.styles
  for _, attachment in ipairs(attachments) do
    if attachment.kind ~= 'image' then
      spans[#spans + 1] =
        { ' [' .. attachment.kind .. ': ' .. attachment.name .. ']', s.blue }
      if attachment.url then
        spans[#spans + 1] = { ' ' .. attachment.url, s.darkgray }
      end
    end
  end
end

--- Appends `(edited)` to a spans table in place. Reactions are no longer
--- appended here; they render on their own row via `render_reactions`.
---@param spans table
---@param event TircEvent
function Theme:append_message_meta(spans, event)
  local s = self.styles
  self:append_attachments(spans, event)
  if event.edited then
    spans[#spans + 1] = { ' (edited)', s.darkgray }
  end
end

--- Builds the clickable reaction pills for a message, one entry per key sorted
--- alphabetically. Each pill is `{ key = <emoji>, spans = TircSpans }`; the
--- renderer lays them out on a dedicated row below the message and uses `key`
--- to map clicks back to the reaction. `hovered_key` is the key of the pill
--- under the mouse for this message (or `nil`), so it can be highlighted.
---@param event TircEvent
---@param hovered_key string|nil
---@return TircReactionPill[]
function Theme:render_reactions(event, hovered_key)
  if not event.reactions then
    return {}
  end
  local s = self.styles
  local keys = {}
  for key in pairs(event.reactions) do
    keys[#keys + 1] = key
  end
  table.sort(keys)
  local pills = {}
  for _, key in ipairs(keys) do
    local reaction = event.reactions[key]
    if reaction.count > 0 then
      local style = s.reaction
      if key == hovered_key then
        style = s.reaction_hover
      elseif reaction.mine then
        style = s.reaction_mine
      end
      pills[#pills + 1] = {
        key = key,
        spans = {
          { ' ' .. key .. ' ' .. tostring(reaction.count) .. ' ', style },
        },
      }
    end
  end
  return pills
end

--- Builds the quick-reaction pills for the selected message, appended onto the
--- same row as `render_reactions` so both read as one unified strip. Each pill is
--- styled exactly like an ordinary reaction pill (including the `reaction_hover`
--- highlight when hovered), so they are visually indistinguishable. Emojis that
--- already have a reaction are skipped here - `render_reactions` renders those
--- (with their count) - to avoid a duplicate pill. `key` is the emoji so a click
--- toggles that reaction. The number keys `1`..`9` map to `emojis` by position,
--- so the first nine pills are prefixed with their shortcut number; the mapping
--- follows the config index, which stays stable even when earlier emojis are
--- skipped for already having a reaction.
---@param event TircEvent
---@param emojis string[]
---@param hovered_key string|nil
---@return TircReactionPill[]
function Theme:render_quick_reactions(event, emojis, hovered_key)
  local s = self.styles
  local reactions = event.reactions or {}
  local pills = {}
  for i, emoji in ipairs(emojis) do
    -- Skip emojis already shown as a counted reaction pill.
    if not (reactions[emoji] and reactions[emoji].count > 0) then
      local style = emoji == hovered_key and s.reaction_hover or s.reaction
      -- Only the first nine positions have a number-key shortcut to label.
      local label = i <= 9 and (tostring(i) .. ' ' .. emoji) or emoji
      pills[#pills + 1] = {
        key = emoji,
        spans = {
          { ' ' .. label .. ' ', style },
        },
      }
    end
  end
  return pills
end

---@param event TircEvent
---@param _nickname string
function Theme:message_text(event, _nickname)
  local kind = event.type

  if event.redacted then
    return { { '[message deleted]', self.styles.darkgray } }
  end

  if kind == 'message' then
    local spans = self:format_message(event)
    self:append_message_meta(spans, event)
    return spans
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
  local s = self.styles
  return {
    self:role_styles()[user.role] or {},
    { user.name, self:nick_style(user, s.blue) },
  }
end

--- Optional whole-row background for a user-list entry, applied by the
--- renderer across the full row width (unlike `user`'s spans, which only
--- paint under the glyphs). Returning nil paints nothing extra.
---
--- Hovering underlines the whole row instead of using a background fill:
--- a bright bg would fight with per-nick colors (e.g. `nick_colors`), while
--- an underline layers on top of whatever foreground the nick already has.
---@param user TircUser
---@return TircThemeStyle?
function Theme:userlist_row_style(user)
  return user.is_hovered and self.styles.user_hover or nil
end

--- Fills a separator line: centers `label` (with a space on each side) within
--- `width` columns using `fill_char` on both sides. Returns left_fill, label,
--- right_fill strings. Falls back gracefully when width <= label length.
---@param label string
---@param width integer
---@param fill_char string single character used for padding
---@return string, string, string
local function centered_separator(label, width, fill_char)
  local inner = ' ' .. label .. ' '
  local remaining = math.max(0, width - #inner)
  local left_n = math.floor(remaining / 2)
  local right_n = remaining - left_n
  return string.rep(fill_char, left_n), inner, string.rep(fill_char, right_n)
end

--- Renders the separator injected between already-read messages and new unread
--- ones. Return `nil` or an empty table to suppress the separator entirely.
---@param width integer pane width in columns
---@return TircSpans
function Theme:render_unread_separator(width)
  local s = self.styles
  local left, label, right = centered_separator('new messages', width, '─')
  return {
    { left, s.unread_separator },
    { label, s.unread_separator },
    { right, s.unread_separator },
  }
end

--- Renders the separator injected between messages from different calendar days.
--- `date` carries year, month, day (and hour/minute/second, unused here).
--- Return `nil` or an empty table to suppress.
---@param date TircDateTime
---@param width integer pane width in columns
---@return TircSpans
function Theme:render_date_separator(date, width)
  local s = self.styles
  local label = date:format('%-d %b %Y')
  local left, inner, right = centered_separator(label, width, '─')
  return {
    { left, s.darkgray },
    { inner, s.twhite },
    { right, s.darkgray },
  }
end

---@param buffer TircBufferTab
local function has_unique_name(buffer)
  local count = 0
  for _, b in ipairs(tirc.buffers) do
    if b.name == buffer.name then
      count = count + 1
    end
  end
  return count <= 1
end

--- Whether a tab must prefix its backend label to disambiguate a duplicated
--- buffer name. Only needed in layouts without visible backend context: the
--- grouped/per-backend/tabbed layouts already show which backend a tab
--- belongs to, so only 'linear' (and unknown custom styles) disambiguate.
---@param buffer TircBufferTab
function Theme:tab_needs_backend_prefix(buffer)
  local style = self:resolved_buffer_bar_style()
  if style == 'grouped' or style == 'per-backend' or style == 'tabbed' then
    return false
  end
  return not has_unique_name(buffer)
end

--- Renders one tab in the buffer bar. Focused tabs are styled brighter;
--- tabs with a mention use red, tabs with unread activity use white.
--- `first` is true when the tab is the first element of its bar row, for
--- themes whose tabs carry leading separators (the default theme ignores it).
---@param buffer TircBufferTab
---@param first? boolean
function Theme:render_buffer_tab(buffer, first)
  local s = self.styles
  local name = self:tab_needs_backend_prefix(buffer)
      and (buffer:backend_label() .. '/' .. buffer.name)
    or buffer.name

  local style
  if buffer:is_hovered() then
    style = s.tab_hover
  elseif buffer:is_focused() then
    style = s.tab_focused
  elseif buffer.has_mention then
    style = s.tab_mention
  elseif buffer.has_unread then
    style = s.tab_unread
  else
    style = s.tab
  end

  name = name .. self:tab_status_suffix(buffer)

  return { { ' ' .. name .. ' ', style }, ' ' }
end

--- Bracketed status/latency suffix for a buffer tab, or '' when there is nothing
--- to show. The status buffer surfaces its backend's connection state
--- ([offline]/[connecting]) and, once connected, round-trip latency over 100ms
--- ([123ms]); a system buffer is marked [server]. Themes append this to the tab
--- name so every layout (and the slanted theme) stays consistent.
---@param buffer TircBufferTab
---@return string
function Theme:tab_status_suffix(buffer)
  if buffer.is_status then
    local conn = buffer.connection_status
    if conn == 'disconnected' then
      return ' [offline]'
    elseif conn == 'connecting' then
      return ' [connecting]'
    elseif buffer.latency_ms and buffer.latency_ms > 100 then
      return ' [' .. buffer.latency_ms .. 'ms]'
    end
  elseif buffer.is_system then
    return ' [server]'
  end
  return ''
end

--- Groups buffers by backend, preserving buffer order within each group and
--- backend order of first appearance. Labels come from `backend_metadata.label`
--- when set, else the backend name.
---@param buffers TircBufferTab[]
---@return { id: integer, label: string, buffers: TircBufferTab[], has_unread: boolean, has_mention: boolean }[]
function Theme:backend_groups(buffers)
  local groups, order = {}, {}
  for _, b in ipairs(buffers) do
    local g = groups[b.backend_id]
    if not g then
      g = {
        id = b.backend_id,
        label = b:backend_label(),
        buffers = {},
        has_unread = false,
        has_mention = false,
      }
      groups[b.backend_id] = g
      order[#order + 1] = g
    end
    g.buffers[#g.buffers + 1] = b
    g.has_unread = g.has_unread or b.has_unread
    g.has_mention = g.has_mention or b.has_mention
  end
  return order
end

--- Renders one backend tab for the tabbed layout's first row. The selected
--- backend is highlighted; unselected backends show mention/unread activity.
--- `first` is true for the row's first element (see `render_buffer_tab`).
---@param group { id: integer, label: string, has_unread: boolean, has_mention: boolean }
---@param first? boolean
function Theme:render_backend_tab(group, first)
  local s = self.styles
  local style
  if tirc.selected_backend == group.id then
    style = s.backend_tab_selected
  elseif group.has_mention then
    style = s.tab_mention
  elseif group.has_unread then
    style = s.tab_unread
  else
    style = s.backend_tab
  end
  return { { ' ' .. group.label .. ' ', style }, ' ' }
end

--- The bar layouts this theme understands, surfaced by `:barstyle`. Subclasses
--- adding layouts should extend this list (and branch in `render_buffer_bar`).
Theme.buffer_bar_styles = { 'linear', 'grouped', 'per-backend', 'tabbed' }

--- The active bar layout: the runtime `:barstyle` override wins, then the
--- `buffer_bar` theme option, then 'linear'.
function Theme:resolved_buffer_bar_style()
  return tirc.buffer_bar_style or self.buffer_bar or 'linear'
end

--- Base background colour painted behind the whole bar, or nil for the
--- terminal default. Override in themes with a custom bar palette.
---@return string?
function Theme:bar_background()
  return nil
end

--- Lays out the whole buffer bar. Returns `{ rows = ..., ids = ... }` (see
--- `TircBufferBar`): one `rows` entry per rendered line, and one `ids` entry
--- per top-level row element declaring what a click on it does (a buffer id,
--- a `backend:`/`backend-select:` marker, or `''` for decoration). Dispatches
--- on the layout resolved by `resolved_buffer_bar_style` - unknown styles
--- render as 'linear'. Override one of the `render_*_bar` methods (or the
--- tab-level `render_buffer_tab`/`render_backend_tab`) for custom looks, or
--- this method for entirely custom layouts.
---@param buffers TircBufferTab[]
---@return TircBufferBar
function Theme:render_buffer_bar(buffers)
  local style = self:resolved_buffer_bar_style()
  local bar
  if style == 'grouped' then
    bar = self:render_grouped_bar(buffers)
  elseif style == 'per-backend' then
    bar = self:render_per_backend_bar(buffers)
  elseif style == 'tabbed' then
    bar = self:render_tabbed_bar(buffers)
  else
    bar = self:render_linear_bar(buffers)
  end
  bar.bg = bar.bg or self:bar_background()
  return bar
end

--- One row of all buffer tabs, in buffer order (the classic layout).
---@param buffers TircBufferTab[]
---@return TircBufferBar
function Theme:render_linear_bar(buffers)
  local row = BarRow.new()
  for _, buffer in ipairs(buffers) do
    row:add(self:render_buffer_tab(buffer, row:first()), buffer.id)
  end
  return BarRow.bar(row)
end

--- One row, buffers grouped behind a clickable backend label:
--- ` libera: (status) #rust │ matrix: friends `. Clicking a label focuses that
--- backend's last-viewed buffer.
---@param buffers TircBufferTab[]
---@return TircBufferBar
function Theme:render_grouped_bar(buffers)
  local s = self.styles
  local row = BarRow.new()
  for i, group in ipairs(self:backend_groups(buffers)) do
    if i > 1 then
      row:add { '│ ', s.darkgray }
    end
    row:add({ group.label .. ': ', s.bar_group_label }, 'backend:' .. group.id)
    for _, buffer in ipairs(group.buffers) do
      row:add(self:render_buffer_tab(buffer), buffer.id)
    end
  end
  return BarRow.bar(row)
end

--- One row per backend, each led by a clickable backend label.
---@param buffers TircBufferTab[]
---@return TircBufferBar
function Theme:render_per_backend_bar(buffers)
  local s = self.styles
  local rows = {}
  for _, group in ipairs(self:backend_groups(buffers)) do
    local row = BarRow.new()
    row:add({ group.label .. ': ', s.bar_group_label }, 'backend:' .. group.id)
    for _, buffer in ipairs(group.buffers) do
      row:add(self:render_buffer_tab(buffer), buffer.id)
    end
    rows[#rows + 1] = row
  end
  return BarRow.bar(unpack(rows))
end

--- Two rows: backend tabs on top, the selected backend's buffers below. The
--- `tabbed_click` option picks what a backend-tab click does ('focus' jumps to
--- its last-viewed buffer, 'select' only switches the visible row).
---@param buffers TircBufferTab[]
---@return TircBufferBar
function Theme:render_tabbed_bar(buffers)
  local groups = self:backend_groups(buffers)
  local selected = tirc.selected_backend or (groups[1] and groups[1].id)
  local marker = self.tabbed_click == 'select' and 'backend-select:'
    or 'backend:'

  local backend_row = BarRow.new()
  local buffer_row = BarRow.new()
  for _, group in ipairs(groups) do
    backend_row:add(
      self:render_backend_tab(group, backend_row:first()),
      marker .. group.id
    )
    if group.id == selected then
      for _, buffer in ipairs(group.buffers) do
        buffer_row:add(
          self:render_buffer_tab(buffer, buffer_row:first()),
          buffer.id
        )
      end
    end
  end

  return BarRow.bar(backend_row, buffer_row)
end

return Theme
