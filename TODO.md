# TODO

What's missing for `tirc` to be a comfortable day-to-day chat client, ordered by
priority. Within each tier, roughly by impact. Tags in `[brackets]` note the area.

- **P0 - Necessary**: daily-driver blockers; it isn't really usable without these.
- **P1 - Expected**: standard features users assume a chat app has.
- **P2 - Convenience**: quality-of-life and polish.
- **P3 - Advanced / future**: bigger or speculative efforts.

_(designed-for)_ = groundwork already in place. _(deferred)_ = intentionally
postponed during the protocol-abstraction work.

## P0 - Necessary

- [x] **Selectable text / copy-paste.** [mouse] Both mechanisms, switchable via
      `config.selection_mode` (`'app'`/`'native'`): a `Ctrl-s` release-capture copy
      mode (terminal-native selection, `-- COPY --` hint) and app-level mouse
      selection with `y`/`Ctrl-c` clipboard yank (linewise, read back from the last
      rendered cell buffer via `arboard`).

## P1 - Expected

- [ ] **Highlight on mention + notification.** [notify] Distinct styling when your
      nick / Matrix display name is mentioned, plus optional terminal bell / desktop
      notification.
- [ ] **Activity / unread indicators** in the buffer bar. [notify]
- [ ] **Matrix history pagination on scroll.** [matrix] Backfill is a one-shot ~30
      (`backfill_room`); paginate older history near the top (`room.messages` `end`
      token).
- [ ] **Matrix inbound edits / redactions / reactions.** [matrix] `State` already
      handles `ChatEvent::Edit/Redaction/Reaction`; the adapter doesn't emit them,
      and reactions aren't rendered (`[message deleted]`/`(edited)` are).
- [x] **Matrix E2E encryption.** [matrix] `e2e-encryption` is enabled (the async
      `Send` overflow is resolved with `#![recursion_limit = "256"]`); the crypto
      store rides the per-account sqlite store, sends auto-encrypt, sync
      auto-decrypts, backfill decrypts (or shows `[unable to decrypt ...]`), and
      startup reports the device/cross-signing posture. _Remaining:_ interactive
      (SAS/QR) device verification from within tirc - incoming requests are only
      surfaced to the status buffer for now.
- [ ] **Nick / room completion.** [input] Tab-complete nicknames, `#channels`, room
      aliases.
- [x] **Input history.** [input] Up/Down to recall previously sent messages.
- [ ] **Multiline input composing.** [input] Input scrolls horizontally only
      (`renderer.rs::render_input`); support soft-wrap / multiline.
- [ ] **SASL authentication.** [irc] Many networks require it (currently plain
      `identify`).
- [ ] **Secret handling.** [config] Passwords (Matrix `password`, IRC) are plaintext
      in `init.lua`; support access tokens, env vars, and/or a keyring.
- [ ] **Scrollable user list.** [layout] Long rosters truncate to viewport height
      (`renderer.rs::render_users`).
- [ ] **Buffer lifecycle.** [layout] No way to close a buffer; buffers aren't pruned
      on part/leave (IRC PART, Matrix leave). Add a close keybind + prune.
- [ ] **Matrix roster/title stay current.** [matrix] Display-name changes are
      dropped (the member handler skips `Join`-with-prior-`Join`; emit
      `ChatEvent::Rename`), and live room renames aren't handled (set once in
      `populate_room`; handle `m.room.name`).
- [ ] **In-buffer search.** [read] `/` to search scrollback.

## P2 - Convenience

- [x] **Clickable buffer bar.** [mouse] Left-click switches; right-click menu
      (mark read, leave, close buffer). Hit boxes are measured from the actual
      rendered bar row so themes with separators (slanted) map correctly.
- [x] **Clickable user list.** [mouse] Left-click opens a query/PM; right-click
      menu (whois, open query, mention). _(op/voice and ignore omitted: no MODE
      `Command` nor ignore list yet - see "Mid-session MODE role changes")_
- [ ] **Clickable URLs** (OSC 8 hyperlinks or a follow-link keybind). [mouse]
- [ ] **Scroll the buffer bar to the focused buffer** so long (Matrix) names aren't
      clipped (`renderer.rs::render_buffer_bar`). [layout]
- [x] **Resizable user-list sidebar** via keybinds (`<`/`>`/`=`) and mouse drag on
      the split boundary (`ViewState::sidebar_width` overrides the default 10%). [layout]
- [ ] **Outbound reactions / redactions / edits** (`Command::React/Redact`). [matrix]
- [ ] **Render Matrix HTML bodies** (`MessageBody.formatted`). [render]
- [ ] **Broader IRC command parity**: ban, names, who. [irc] _(kick, invite,
      topic, /away done)_
- [ ] **Mid-session MODE role changes** (op/voice without a rejoin; parse
      `ChannelMODE +o/+v`). [irc]
- [ ] **Jump to first-unread / read marker** per buffer. [read]
- [ ] **Richer status line** (mode, current buffer, lag, activity). [render]
- [x] **Render only on state change** (dirty flag); avoids redraw on idle ticks
      (CODE_REVIEW.md §3.5). [perf] _(`InputHandler::dirty` set per event;
      `main` renders only on `take_dirty`, with a ~5s heartbeat repaint as a
      safety net.)_
- [ ] **`log`/`tracing` to a file** under XDG state; never print to the TUI. See
      memory `logging-use-log-crate`. [infra]
- [ ] **CI**: `cargo test` + `clippy -D warnings` + `stylua --check` + a dockerized
      Matrix integration job. [infra]
- [ ] **Friendlier config errors** (name the offending server/field; `protocol` is
      now mandatory and errors bare). [config]
- [ ] **Expose Matrix `store_dir` / device id** in the Lua config. [config]
- [ ] **Per-network chat logging to disk.** [config]
- [ ] **Theme cookbook / docs** for the extensible `TircTheme` class, plus
      `_tirc`/`tirc.date_time` type stubs in the exported `types/`. [docs]
- [ ] **Readline-ish editing** niceties (word motions, kill/yank). [input]

## P3 - Advanced / future

- [ ] **Split panes / windows** (tmux/neovim-style). _(designed-for: domain vs
      `ViewState` split already separates focus from the buffer model.)_ [layout]
- [ ] **Daemon / relay split** (headless backends, attachable TUI). _(designed-for:
      the normalized `ChatEvent`/`Command` model is `serde`-serializable.)_ [arch]
- [ ] **Broader Lua plugin/event API.** Only the `event` hook
      (`config::EventName::Event`) and `send_message`/`send_notice` exist; add
      lifecycle hooks and richer commands. [arch]
- [ ] **Matrix threads / replies, typing indicators, read receipts, presence.**
      [matrix]
- [ ] **DMs, invites UX, navigable room directory** (`:list` exists). [matrix]
- [ ] **Config hot-reload** without restart. [config]
- [ ] **Buffer reordering / pinning / grouping by network.** [layout]
- [ ] **Remaining CODE_REVIEW polish**: `Renderer` zero-field struct, version table
      via `Serialize`, trim `wrap.rs` dead-code `#[allow]`s. [arch]
- [ ] **CTCP / DCC.** [irc]
