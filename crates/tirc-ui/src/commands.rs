//! The command registry: the single declarative table describing every
//! builtin `:` command (canonical name, aliases, argument arity, per-argument
//! completion kind) plus vim-style name resolution.
//!
//! The dispatcher in the binary matches on [`BuiltinCmd`] so a spec without a
//! handler arm is a compile error; command-mode completion derives its
//! candidates from [`BUILTIN_COMMANDS`]. Resolution follows vim: an exact
//! canonical name or alias wins, otherwise a unique prefix of a canonical
//! name executes, an ambiguous prefix reports the candidates, and anything
//! else is unknown. Lua user commands (registered via `tirc.create_command`)
//! participate through the `lua_names` argument to [`resolve`]; builtins
//! shadow them on exact match.

/// Every builtin command, matched exhaustively by the dispatcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinCmd {
    Quit,
    Msg,
    Me,
    Describe,
    Notice,
    Join,
    Part,
    Nick,
    Whois,
    Topic,
    Away,
    Kick,
    Invite,
    Alias,
    Unalias,
    Bufmove,
    Barstyle,
    List,
    Verify,
    Redraw,
    Debug,
    Reload,
}

/// Argument arity, mirroring vim's `-nargs`: `0`, `1`, `?`, `*`, `+`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nargs {
    Zero,
    One,
    ZeroOrOne,
    Any,
    AtLeastOne,
}

impl Nargs {
    pub fn takes_args(self) -> bool {
        !matches!(self, Nargs::Zero)
    }

    /// Whether an argument must be present.
    pub fn requires_arg(self) -> bool {
        matches!(self, Nargs::One | Nargs::AtLeastOne)
    }
}

/// What completes at a given argument position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgKind {
    /// Free text; no completion.
    None,
    /// Buffer targets of the focused backend (skipping the status buffer).
    Channel,
    /// Members of the focused buffer.
    Nick,
    /// All buffer labels.
    Buffer,
    /// A fixed choice list.
    Choices(&'static [&'static str]),
    /// The theme's `buffer_bar_styles` plus `"reset"` (resolved via Lua).
    BarStyle,
}

/// One builtin command's metadata.
#[derive(Debug)]
pub struct CommandSpec {
    pub cmd: BuiltinCmd,
    /// Canonical name, e.g. `"quit"`. Prefix resolution matches against this.
    pub name: &'static str,
    /// Exact-match abbreviations, e.g. `&["q"]`. Never prefix-matched.
    pub aliases: &'static [&'static str],
    pub nargs: Nargs,
    /// Completion kind per argument position; positions past the end do not
    /// complete (the last entry does not repeat).
    pub args: &'static [ArgKind],
    pub description: &'static str,
}

pub const BUILTIN_COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        cmd: BuiltinCmd::Quit,
        name: "quit",
        aliases: &["q"],
        nargs: Nargs::Zero,
        args: &[],
        description: "Disconnect all backends and exit",
    },
    CommandSpec {
        cmd: BuiltinCmd::Msg,
        name: "msg",
        aliases: &["m"],
        nargs: Nargs::AtLeastOne,
        args: &[ArgKind::Nick, ArgKind::None],
        description: "Open a conversation and optionally send a message",
    },
    CommandSpec {
        cmd: BuiltinCmd::Me,
        name: "me",
        aliases: &[],
        nargs: Nargs::AtLeastOne,
        args: &[ArgKind::None],
        description: "Send an action to the focused buffer",
    },
    CommandSpec {
        cmd: BuiltinCmd::Describe,
        name: "describe",
        aliases: &["desc"],
        nargs: Nargs::AtLeastOne,
        args: &[ArgKind::Nick, ArgKind::None],
        description: "Send an action to a target",
    },
    CommandSpec {
        cmd: BuiltinCmd::Notice,
        name: "notice",
        aliases: &[],
        nargs: Nargs::AtLeastOne,
        args: &[ArgKind::Nick, ArgKind::None],
        description: "Send a notice to a target",
    },
    CommandSpec {
        cmd: BuiltinCmd::Join,
        name: "join",
        aliases: &["j"],
        nargs: Nargs::One,
        args: &[ArgKind::Channel],
        description: "Join a channel",
    },
    CommandSpec {
        cmd: BuiltinCmd::Part,
        name: "part",
        aliases: &["p"],
        nargs: Nargs::One,
        args: &[ArgKind::Channel],
        description: "Leave a channel",
    },
    CommandSpec {
        cmd: BuiltinCmd::Nick,
        name: "nick",
        aliases: &["n"],
        nargs: Nargs::One,
        args: &[ArgKind::Nick],
        description: "Change your nickname",
    },
    CommandSpec {
        cmd: BuiltinCmd::Whois,
        name: "whois",
        aliases: &[],
        nargs: Nargs::One,
        args: &[ArgKind::Nick],
        description: "Query information about a user",
    },
    CommandSpec {
        cmd: BuiltinCmd::Topic,
        name: "topic",
        aliases: &[],
        nargs: Nargs::AtLeastOne,
        args: &[ArgKind::None],
        description: "Set the topic of the focused channel",
    },
    CommandSpec {
        cmd: BuiltinCmd::Away,
        name: "away",
        aliases: &[],
        nargs: Nargs::Any,
        args: &[ArgKind::None],
        description: "Set or clear your away message",
    },
    CommandSpec {
        cmd: BuiltinCmd::Kick,
        name: "kick",
        aliases: &[],
        nargs: Nargs::AtLeastOne,
        args: &[ArgKind::Channel, ArgKind::Nick, ArgKind::None],
        description: "Kick a user from a channel",
    },
    CommandSpec {
        cmd: BuiltinCmd::Invite,
        name: "invite",
        aliases: &[],
        nargs: Nargs::AtLeastOne,
        args: &[ArgKind::Nick, ArgKind::Channel],
        description: "Invite a user to a channel",
    },
    CommandSpec {
        cmd: BuiltinCmd::Alias,
        name: "alias",
        aliases: &[],
        nargs: Nargs::AtLeastOne,
        args: &[ArgKind::None],
        description: "Set a display alias for the focused buffer",
    },
    CommandSpec {
        cmd: BuiltinCmd::Unalias,
        name: "unalias",
        aliases: &[],
        nargs: Nargs::Zero,
        args: &[],
        description: "Remove the focused buffer's display alias",
    },
    CommandSpec {
        cmd: BuiltinCmd::Bufmove,
        name: "bufmove",
        aliases: &[],
        nargs: Nargs::One,
        args: &[ArgKind::None],
        description: "Move the focused buffer in the buffer bar",
    },
    CommandSpec {
        cmd: BuiltinCmd::Barstyle,
        name: "barstyle",
        aliases: &[],
        nargs: Nargs::ZeroOrOne,
        args: &[ArgKind::BarStyle],
        description: "Show or override the buffer bar style",
    },
    CommandSpec {
        cmd: BuiltinCmd::List,
        name: "list",
        aliases: &[],
        nargs: Nargs::Zero,
        args: &[],
        description: "List channels on the focused backend",
    },
    CommandSpec {
        cmd: BuiltinCmd::Verify,
        name: "verify",
        aliases: &[],
        nargs: Nargs::ZeroOrOne,
        args: &[ArgKind::Choices(&["accept", "confirm", "cancel", "reject"])],
        description: "Start or answer an interactive verification",
    },
    CommandSpec {
        cmd: BuiltinCmd::Redraw,
        name: "redraw",
        aliases: &[],
        nargs: Nargs::Zero,
        args: &[],
        description: "Redraw the screen",
    },
    CommandSpec {
        cmd: BuiltinCmd::Debug,
        name: "debug",
        aliases: &[],
        nargs: Nargs::Zero,
        args: &[],
        description: "Toggle the debug panel",
    },
    CommandSpec {
        cmd: BuiltinCmd::Reload,
        name: "reload",
        aliases: &[],
        nargs: Nargs::Zero,
        args: &[],
        description: "Reload the config and theme",
    },
];

/// The outcome of resolving a typed command name.
#[derive(Clone, Debug, PartialEq)]
pub enum Resolution<'a> {
    Builtin(&'a CommandSpec),
    /// A Lua user command, by its registered name.
    Lua(String),
    /// The prefix matched several commands; sorted candidate names.
    Ambiguous(Vec<String>),
    Unknown,
}

impl PartialEq for CommandSpec {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

/// Resolves `name` vim-style: exact canonical name or alias first (builtin
/// before Lua), then a prefix match over the union of builtin canonical names
/// and Lua command names. Aliases never prefix-match.
pub fn resolve(name: &str, lua_names: &[String]) -> Resolution<'static> {
    if name.is_empty() {
        return Resolution::Unknown;
    }
    if let Some(spec) = BUILTIN_COMMANDS
        .iter()
        .find(|s| s.name == name || s.aliases.contains(&name))
    {
        return Resolution::Builtin(spec);
    }
    if lua_names.iter().any(|n| n == name) {
        return Resolution::Lua(name.to_string());
    }

    let builtins: Vec<&'static CommandSpec> = BUILTIN_COMMANDS
        .iter()
        .filter(|s| s.name.starts_with(name))
        .collect();
    let lua: Vec<&String> = lua_names.iter().filter(|n| n.starts_with(name)).collect();
    match (builtins.len(), lua.len()) {
        (0, 0) => Resolution::Unknown,
        (1, 0) => Resolution::Builtin(builtins[0]),
        (0, 1) => Resolution::Lua(lua[0].clone()),
        _ => {
            let mut names: Vec<String> = builtins
                .iter()
                .map(|s| s.name.to_string())
                .chain(lua.iter().map(|n| (*n).clone()))
                .collect();
            names.sort();
            names.dedup();
            Resolution::Ambiguous(names)
        }
    }
}

/// Splits a command line into the command name and the rest (empty when the
/// line has no arguments). The rest keeps its internal spacing.
pub fn split_line(line: &str) -> (&str, &str) {
    match line.split_once(' ') {
        Some((name, rest)) => (name, rest),
        None => (line, ""),
    }
}

/// Validates the argument string against a spec's arity. `One` only requires
/// presence - multi-word rests are passed through, matching the historical
/// tolerance of the hand-rolled parser.
pub fn check_nargs(spec: &CommandSpec, rest: &str) -> Result<(), String> {
    if spec.nargs == Nargs::Zero && !rest.trim().is_empty() {
        return Err(format!(
            "Trailing characters: :{} takes no arguments",
            spec.name
        ));
    }
    if spec.nargs.requires_arg() && rest.trim().is_empty() {
        return Err(format!("Argument required for :{}", spec.name));
    }
    Ok(())
}

/// Locates the argument word under the cursor: its zero-based argument index,
/// the char span covering the whole word (so accepting mid-word replaces it),
/// and the query text between the word start and the cursor. `None` while the
/// cursor is still inside the first word (command-name territory). A cursor
/// in whitespace yields an empty word at the cursor. All indices are char
/// indices.
pub fn arg_word_span(value: &str, cursor: usize) -> Option<(usize, usize, usize, String)> {
    let chars: Vec<char> = value.chars().collect();
    let cursor = cursor.min(chars.len());
    let first_word_len = chars
        .iter()
        .position(|c| c.is_whitespace())
        .unwrap_or(chars.len());
    if cursor <= first_word_len {
        return None;
    }

    let mut i = first_word_len;
    let mut arg_index = 0;
    loop {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if cursor < i || i >= chars.len() {
            // The cursor sits in whitespace (or at the end of the line past
            // any word): a fresh, empty argument word at the cursor.
            return Some((arg_index, cursor, cursor, String::new()));
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        if cursor <= i {
            let query: String = chars[start..cursor].iter().collect();
            return Some((arg_index, start, i, query));
        }
        arg_index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_builtin(name: &str) -> &'static CommandSpec {
        match resolve(name, &[]) {
            Resolution::Builtin(spec) => spec,
            other => panic!("expected builtin for {name:?}, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_exact_name_and_alias() {
        assert_eq!(resolve_builtin("quit").cmd, BuiltinCmd::Quit);
        assert_eq!(resolve_builtin("q").cmd, BuiltinCmd::Quit);
        assert_eq!(resolve_builtin("j").cmd, BuiltinCmd::Join);
    }

    #[test]
    fn test_resolve_exact_alias_beats_prefix() {
        // "n" is the nick alias even though it prefixes both nick and notice.
        assert_eq!(resolve_builtin("n").cmd, BuiltinCmd::Nick);
        // "m" is the msg alias even though it prefixes both msg and me.
        assert_eq!(resolve_builtin("m").cmd, BuiltinCmd::Msg);
    }

    #[test]
    fn test_resolve_unique_prefix() {
        assert_eq!(resolve_builtin("wh").cmd, BuiltinCmd::Whois);
        assert_eq!(resolve_builtin("rel").cmd, BuiltinCmd::Reload);
        assert_eq!(resolve_builtin("to").cmd, BuiltinCmd::Topic);
    }

    #[test]
    fn test_resolve_ambiguous_prefix() {
        match resolve("re", &[]) {
            Resolution::Ambiguous(names) => {
                assert_eq!(names, ["redraw", "reload"]);
            }
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_unknown() {
        assert_eq!(resolve("xyzzy", &[]), Resolution::Unknown);
        assert_eq!(resolve("", &[]), Resolution::Unknown);
    }

    #[test]
    fn test_resolve_lua_names() {
        let lua = vec!["shrug".to_string(), "slap".to_string()];
        assert_eq!(resolve("shrug", &lua), Resolution::Lua("shrug".into()));
        assert_eq!(resolve("sh", &lua), Resolution::Lua("shrug".into()));
        match resolve("s", &lua) {
            Resolution::Ambiguous(names) => assert_eq!(names, ["shrug", "slap"]),
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_builtin_shadows_lua_on_exact_match() {
        let lua = vec!["quit".to_string(), "q".to_string()];
        for name in ["quit", "q"] {
            match resolve(name, &lua) {
                Resolution::Builtin(spec) => assert_eq!(spec.cmd, BuiltinCmd::Quit),
                other => panic!("expected builtin for {name:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_resolve_prefix_union_includes_lua() {
        // "ki" prefixes builtin kick and a Lua kickban: ambiguous.
        let lua = vec!["kickban".to_string()];
        match resolve("ki", &lua) {
            Resolution::Ambiguous(names) => assert_eq!(names, ["kick", "kickban"]),
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn test_registry_names_and_aliases_unique() {
        let mut seen = std::collections::HashSet::new();
        for spec in BUILTIN_COMMANDS {
            assert!(!spec.name.is_empty());
            assert!(seen.insert(spec.name), "duplicate name {}", spec.name);
            for alias in spec.aliases {
                assert!(!alias.is_empty());
                assert!(seen.insert(alias), "duplicate alias {alias}");
            }
        }
    }

    #[test]
    fn test_split_line() {
        assert_eq!(split_line("quit"), ("quit", ""));
        assert_eq!(split_line("msg bob hi there"), ("msg", "bob hi there"));
        assert_eq!(split_line("msg  bob"), ("msg", " bob"));
    }

    #[test]
    fn test_check_nargs() {
        let quit = resolve_builtin("quit");
        assert!(check_nargs(quit, "").is_ok());
        assert!(check_nargs(quit, "now").is_err());

        let join = resolve_builtin("join");
        assert!(check_nargs(join, "#rust").is_ok());
        assert!(check_nargs(join, "").is_err());

        let away = resolve_builtin("away");
        assert!(check_nargs(away, "").is_ok());
        assert!(check_nargs(away, "afk").is_ok());
    }

    #[test]
    fn test_arg_word_span_inside_first_word() {
        assert_eq!(arg_word_span("join", 4), None);
        assert_eq!(arg_word_span("join #rust", 2), None);
        assert_eq!(arg_word_span("", 0), None);
    }

    #[test]
    fn test_arg_word_span_first_arg() {
        // "join " with cursor at the end: empty first argument.
        assert_eq!(arg_word_span("join ", 5), Some((0, 5, 5, String::new())));
        // Cursor mid-word: span covers the whole word, query stops at cursor.
        assert_eq!(
            arg_word_span("join #rust", 7),
            Some((0, 5, 10, "#r".to_string()))
        );
    }

    #[test]
    fn test_arg_word_span_later_args() {
        assert_eq!(
            arg_word_span("kick #chan ni", 13),
            Some((1, 11, 13, "ni".to_string()))
        );
        assert_eq!(
            arg_word_span("kick #chan nick reason", 22),
            Some((2, 16, 22, "reason".to_string()))
        );
    }

    #[test]
    fn test_arg_word_span_in_whitespace() {
        // Cursor between two spaces: a fresh empty word at the cursor.
        assert_eq!(arg_word_span("join  #a", 5), Some((0, 5, 5, String::new())));
        // Cursor in trailing whitespace after a complete word.
        assert_eq!(
            arg_word_span("kick #chan ", 11),
            Some((1, 11, 11, String::new()))
        );
    }

    #[test]
    fn test_arg_word_span_multibyte() {
        // "msg käse" - cursor after the umlaut, char indices throughout.
        assert_eq!(
            arg_word_span("msg käse", 6),
            Some((0, 4, 8, "kä".to_string()))
        );
    }
}
