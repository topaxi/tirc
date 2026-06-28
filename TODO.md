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

- [ ] **Selectable text / copy-paste.** [mouse] Mouse capture suppresses the
      terminal's native selection. Add a copy/selection mode (release capture,
      tmux-style) and/or app-level selection + clipboard yank. _(deferred: conflicts
      with scroll mouse handling added in the scrollback work)_

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

- [ ] **Clickable buffer bar.** [mouse] Click to switch; right-click menu (close,
      mark read, leave).
- [ ] **Clickable user list.** [mouse] Left-click to open a query/PM; right-click
      menu (whois/profile, op/voice, ignore, mention).
- [ ] **Clickable URLs** (OSC 8 hyperlinks or a follow-link keybind). [mouse]
- [ ] **Scroll the buffer bar to the focused buffer** so long (Matrix) names aren't
      clipped (`renderer.rs::render_buffer_bar`). [layout]
- [ ] **Resizable user-list sidebar** via keybinds / mouse drag (fixed 10% split in
      `renderer.rs::render`). [layout]
- [ ] **Outbound reactions / redactions / edits** (`Command::React/Redact`). [matrix]
- [ ] **Render Matrix HTML bodies** (`MessageBody.formatted`). [render]
- [ ] **Broader IRC command parity**: ban, names, who. [irc] _(kick, invite,
      topic, /away done)_
- [ ] **Mid-session MODE role changes** (op/voice without a rejoin; parse
      `ChannelMODE +o/+v`). [irc]
- [ ] **Jump to first-unread / read marker** per buffer. [read]
- [ ] **Richer status line** (mode, current buffer, lag, activity). [render]
- [ ] **Render only on state change** (dirty flag); avoids redraw on idle ticks
      (CODE_REVIEW.md §3.5). [perf]
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
