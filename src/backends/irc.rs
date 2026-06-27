//! IRC backend: the sole place that touches the `irc` crate.
//!
//! Incoming `irc::proto::Message`s are translated into normalized [`ChatEvent`]s
//! (this subsumes the old `to_lua_message` shaping and the `get_target_buffer_name`
//! routing), and outgoing [`Command`]s are translated into IRC sends.

use futures::prelude::*;
use irc::client::prelude::{Capability, Client, Config};
use irc::proto::{message::Tag, Command as IrcCommand, Message, Response};

use crate::core::{
    BackendEvent, BackendId, BackendMessage, ChatEvent, Command, MemberRole, MembershipChange,
    MessageBody, MsgKind, Protocol, TargetId, TxnId, UserRef,
};

use super::{BackendInfo, ChatBackend, CommandReceiver, EventSender};

/// CTCP ACTION wrapper byte sequence (`\x01ACTION <text>\x01`).
const ACTION_PREFIX: &str = "\u{1}ACTION ";

/// Connection parameters for an IRC backend, built from the user config.
#[derive(Clone, Debug)]
pub struct IrcBackendConfig {
    pub host: String,
    pub port: u16,
    pub use_tls: bool,
    pub accept_invalid_cert: bool,
    pub nickname: Vec<String>,
    pub realname: Option<String>,
    pub autojoin: Vec<String>,
}

pub struct IrcBackend {
    id: BackendId,
    config: IrcBackendConfig,
}

impl IrcBackend {
    pub fn new(id: BackendId, config: IrcBackendConfig) -> Self {
        IrcBackend { id, config }
    }

    fn irc_config(&self) -> anyhow::Result<Config> {
        let nickname = self
            .config
            .nickname
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("IRC server '{}' has an empty nickname list", self.config.host))?;

        Ok(Config {
            nickname: Some(nickname),
            alt_nicks: self.config.nickname[1..].to_vec(),
            realname: self.config.realname.clone(),
            server: Some(self.config.host.clone()),
            port: Some(self.config.port),
            use_tls: Some(self.config.use_tls),
            dangerously_accept_invalid_certs: Some(self.config.accept_invalid_cert),
            channels: self.config.autojoin.clone(),
            version: Some(format!(
                "tirc v{} - https://github.com/topaxi/tirc",
                env!("CARGO_PKG_VERSION")
            )),
            ..Default::default()
        })
    }
}

#[async_trait::async_trait]
impl ChatBackend for IrcBackend {
    fn info(&self) -> BackendInfo {
        BackendInfo {
            id: self.id,
            protocol: Protocol::Irc,
            name: self.config.host.clone(),
        }
    }

    async fn run(
        self: Box<Self>,
        events: EventSender,
        mut commands: CommandReceiver,
    ) -> anyhow::Result<()> {
        let id = self.id;
        let mut client = Client::from_config(self.irc_config()?).await?;
        let mut stream = client.stream()?;

        client.send_cap_req(&[
            Capability::EchoMessage,
            Capability::MultiPrefix,
            Capability::ExtendedJoin,
            Capability::AwayNotify,
            Capability::ChgHost,
            Capability::AccountNotify,
            Capability::ServerTime,
            Capability::UserhostInNames,
            Capability::Batch,
            Capability::Custom("labeled-response"),
        ])?;
        client.identify()?;

        let emit = |event: ChatEvent| {
            let _ = events.send(BackendMessage {
                backend: id,
                event: BackendEvent::Event(event),
            });
        };

        loop {
            tokio::select! {
                incoming = stream.next() => {
                    let message = match incoming {
                        Some(Ok(message)) => message,
                        Some(Err(err)) => return Err(err.into()),
                        None => break,
                    };

                    let nickname = client.current_nickname().to_string();

                    // Surface connection readiness on the welcome numeric.
                    if matches!(&message.command, IrcCommand::Response(Response::RPL_WELCOME, _)) {
                        let _ = events.send(BackendMessage {
                            backend: id,
                            event: BackendEvent::Ready { nickname: nickname.clone() },
                        });
                    }

                    for event in translate(&message, &nickname) {
                        emit(event);
                    }
                }
                command = commands.recv() => {
                    match command {
                        Some(command) => apply_command(&client, &nickname_of(&client), command, &emit)?,
                        None => break,
                    }
                }
            }
        }

        Ok(())
    }
}

fn nickname_of(client: &Client) -> String {
    client.current_nickname().to_string()
}

/// Applies an outgoing [`Command`] to the IRC client and emits an optimistic
/// local echo for sends so the message appears immediately. The server's
/// echo-message reply (carrying the same `label`/`TxnId`) later replaces it.
fn apply_command(
    client: &Client,
    nickname: &str,
    command: Command,
    emit: &impl Fn(ChatEvent),
) -> anyhow::Result<()> {
    match command {
        Command::SendMessage {
            target,
            body,
            kind,
            txn,
        } => {
            let wire = match kind {
                MsgKind::Action => format!("{ACTION_PREFIX}{body}\u{1}"),
                _ => body.clone(),
            };

            let irc_command = match kind {
                MsgKind::Notice => IrcCommand::NOTICE(target.0.clone(), wire),
                _ => IrcCommand::PRIVMSG(target.0.clone(), wire),
            };

            let mut message: Message = irc_command.into();
            message.tags = Some(vec![Tag("label".to_string(), Some(txn.0.to_string()))]);
            client.send(message)?;

            emit(ChatEvent::Message {
                target,
                id: None,
                sender: UserRef::new(nickname.to_string()),
                body: MessageBody::plain(body),
                kind,
                echo_of: Some(txn),
            });
        }
        Command::Join { target } => client.send_join(&target.0)?,
        Command::Part { target, reason } => match reason {
            Some(reason) => client.send(IrcCommand::PART(target.0, Some(reason)))?,
            None => client.send_part(&target.0)?,
        },
        Command::SetTopic { target, topic } => {
            client.send(IrcCommand::TOPIC(target.0, Some(topic)))?
        }
        Command::SetNick { nick } => client.send(IrcCommand::NICK(nick))?,
        Command::Whois { user } => client.send(IrcCommand::WHOIS(None, user))?,
        Command::ListChannels => client.send(IrcCommand::LIST(None, None))?,
        Command::Quit { reason } => client.send_quit(reason.unwrap_or_default())?,
        // IRC has no native reactions or message deletion.
        Command::React { .. } | Command::Redact { .. } => {}
    }

    Ok(())
}

/// Strips a leading IRC `label` tag value, parsed as a [`TxnId`], for local-echo
/// correlation.
fn label_txn(message: &Message) -> Option<TxnId> {
    message.tags.as_ref()?.iter().find_map(|tag| {
        if tag.0 == "label" {
            tag.1.as_ref()?.parse::<u64>().ok().map(TxnId)
        } else {
            None
        }
    })
}

/// Decodes a CTCP ACTION payload, returning the inner text when present.
fn strip_action(text: &str) -> Option<&str> {
    text.strip_prefix(ACTION_PREFIX)
        .map(|rest| rest.strip_suffix('\u{1}').unwrap_or(rest))
}

/// Computes the destination buffer for a message, mirroring the original IRC
/// routing rules (self-echo, server-prefix, outgoing without prefix). `None`
/// means the channel/sender target; the caller resolves the status fallback.
fn target_for(message: &Message, nickname: &str, fallback: &str) -> TargetId {
    let buffer = match message.source_nickname() {
        // Incoming message from someone else: a channel message goes to the
        // channel, a direct message to the sender's nick.
        Some(source) if source != nickname => message
            .response_target()
            .unwrap_or(source)
            .to_owned(),
        // Echo of our own message (server replied with our nick as source): file
        // it under the conversation partner.
        Some(_) => fallback.to_owned(),
        // No nick prefix: our own outgoing (no prefix) belongs with its
        // recipient; a server-name prefix belongs in the status buffer.
        None if message.prefix.is_none() => fallback.to_owned(),
        None => TargetId::STATUS.to_owned(),
    };

    if buffer == "*" {
        TargetId::status()
    } else {
        TargetId(buffer)
    }
}

/// Maps an IRC nick-prefix character (as seen in NAMES replies) to a role.
fn role_from_prefix(prefix: char) -> Option<MemberRole> {
    match prefix {
        '~' => Some(MemberRole::Owner),
        '&' => Some(MemberRole::Admin),
        '@' => Some(MemberRole::Op),
        '%' => Some(MemberRole::HalfOp),
        '+' => Some(MemberRole::Voice),
        _ => None,
    }
}

/// Splits an entry from an RPL_NAMREPLY list into its highest role and bare nick.
/// With `multi-prefix`, entries may carry several prefixes (e.g. `@+nick`); the
/// first (highest) wins.
fn parse_names_entry(entry: &str) -> (MemberRole, &str) {
    let mut role = MemberRole::Member;
    let mut rest = entry;

    if let Some(first) = rest.chars().next() {
        if let Some(parsed) = role_from_prefix(first) {
            role = parsed;
        }
    }

    rest = rest.trim_start_matches(|c| role_from_prefix(c).is_some());
    (role, rest)
}

/// Translates an incoming IRC message into zero or more normalized events. Most
/// messages map to one event; an RPL_NAMREPLY expands to one per listed user,
/// and protocol housekeeping (PING, CAP, ...) maps to none.
fn translate(message: &Message, nickname: &str) -> Vec<ChatEvent> {
    if let IrcCommand::Response(Response::RPL_NAMREPLY, args) = &message.command {
        // `<client> <symbol> <channel> :<prefixed nicks>`
        let channel = match args.get(2) {
            Some(channel) => TargetId(channel.clone()),
            None => return Vec::new(),
        };
        let names = args.last().map(String::as_str).unwrap_or_default();

        return names
            .split_whitespace()
            .map(|entry| {
                let (role, nick) = parse_names_entry(entry);
                ChatEvent::Membership {
                    target: channel.clone(),
                    who: UserRef::new(nick.to_string()),
                    change: MembershipChange::Present { role },
                }
            })
            .collect();
    }

    translate_one(message, nickname).into_iter().collect()
}

/// The single-event cases, factored out so `translate` can special-case the
/// multi-event NAMES reply.
fn translate_one(message: &Message, nickname: &str) -> Option<ChatEvent> {
    let raw = || message.to_string().trim_end().to_string();

    match &message.command {
        // RPL_NAMREPLY seeds the roster; handled in `translate` for the
        // multi-user expansion. Match here to suppress the generic numeric.
        IrcCommand::Response(Response::RPL_NAMREPLY, _)
        | IrcCommand::Response(Response::RPL_ENDOFNAMES, _) => None,
        IrcCommand::PRIVMSG(target, text) => {
            let source = message.source_nickname();
            let target_id = target_for(message, nickname, target);

            // A server-prefixed PRIVMSG (no source nick) is informational.
            if source.is_none() && message.prefix.is_some() {
                return Some(ChatEvent::ServerInfo {
                    target: Some(target_id),
                    code: Some("PRIVMSG".to_string()),
                    text: text.clone(),
                    raw: Some(raw()),
                });
            }

            let (kind, body) = match strip_action(text) {
                Some(action) => (MsgKind::Action, action.to_string()),
                None => (MsgKind::Text, text.clone()),
            };

            Some(ChatEvent::Message {
                target: target_id,
                id: None,
                sender: UserRef::new(source.unwrap_or(nickname).to_string()),
                body: MessageBody::plain(body),
                kind,
                echo_of: label_txn(message),
            })
        }
        IrcCommand::NOTICE(target, text) => {
            let source = message.source_nickname();
            let target_id = target_for(message, nickname, target);

            // Server notices (no source nick) render as status info.
            match source {
                Some(source) => Some(ChatEvent::Message {
                    target: target_id,
                    id: None,
                    sender: UserRef::new(source.to_string()),
                    body: MessageBody::plain(text.clone()),
                    kind: MsgKind::Notice,
                    echo_of: label_txn(message),
                }),
                None => Some(ChatEvent::ServerInfo {
                    target: Some(target_id),
                    code: Some("NOTICE".to_string()),
                    text: text.clone(),
                    raw: Some(raw()),
                }),
            }
        }
        IrcCommand::JOIN(channel, _, _) => Some(ChatEvent::Membership {
            target: TargetId(channel.clone()),
            who: UserRef::new(message.source_nickname()?.to_string()),
            change: MembershipChange::Join,
        }),
        IrcCommand::PART(channel, reason) => Some(ChatEvent::Membership {
            target: TargetId(channel.clone()),
            who: UserRef::new(message.source_nickname()?.to_string()),
            change: MembershipChange::Part {
                reason: reason.clone(),
            },
        }),
        IrcCommand::QUIT(reason) => Some(ChatEvent::Quit {
            who: UserRef::new(message.source_nickname()?.to_string()),
            reason: reason.clone(),
        }),
        IrcCommand::NICK(new) => Some(ChatEvent::Rename {
            who: UserRef::new(message.source_nickname()?.to_string()),
            new_display: new.clone(),
        }),
        IrcCommand::TOPIC(channel, topic) => Some(ChatEvent::Topic {
            target: TargetId(channel.clone()),
            who: message.source_nickname().map(|n| UserRef::new(n.to_string())),
            topic: topic.clone().unwrap_or_default(),
        }),
        IrcCommand::ChannelMODE(channel, _) => Some(ChatEvent::ServerInfo {
            target: Some(TargetId(channel.clone())),
            code: Some("MODE".to_string()),
            text: raw(),
            raw: Some(raw()),
        }),
        IrcCommand::Response(response, args) => Some(ChatEvent::ServerInfo {
            target: None,
            code: Some(format!("{response:?}")),
            text: args.last().cloned().unwrap_or_default(),
            raw: Some(raw()),
        }),
        // PING/PONG/CAP/etc. carry no user-facing content.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Message {
        raw.parse().expect("valid irc message")
    }

    fn translate_raw(nickname: &str, raw: &str) -> Option<ChatEvent> {
        translate_one(&parse(raw), nickname)
    }

    fn target_of(event: &ChatEvent) -> &str {
        match event {
            ChatEvent::Message { target, .. }
            | ChatEvent::Membership { target, .. }
            | ChatEvent::Topic { target, .. } => target.as_str(),
            ChatEvent::ServerInfo { target, .. } => {
                target.as_ref().map(TargetId::as_str).unwrap_or(TargetId::STATUS)
            }
            _ => TargetId::STATUS,
        }
    }

    #[test]
    fn incoming_channel_message_targets_channel() {
        let event = translate_raw("me", ":alice!u@h PRIVMSG #tirc :hi").unwrap();
        assert_eq!(target_of(&event), "#tirc");
        assert!(matches!(event, ChatEvent::Message { kind: MsgKind::Text, .. }));
    }

    #[test]
    fn incoming_direct_message_targets_sender() {
        let event = translate_raw("me", ":alice!u@h PRIVMSG me :hi").unwrap();
        assert_eq!(target_of(&event), "alice");
    }

    #[test]
    fn self_echo_targets_partner() {
        let event = translate_raw("me", ":me!u@h PRIVMSG me :hi").unwrap();
        assert_eq!(target_of(&event), "me");
    }

    #[test]
    fn server_notice_goes_to_status() {
        let event = translate_raw("me", ":irc.example.com NOTICE * :Welcome").unwrap();
        assert_eq!(target_of(&event), TargetId::STATUS);
        assert!(matches!(event, ChatEvent::ServerInfo { .. }));
    }

    #[test]
    fn action_is_detected() {
        let event = translate_raw("me", ":alice!u@h PRIVMSG #tirc :\u{1}ACTION waves\u{1}").unwrap();
        match event {
            ChatEvent::Message { kind, body, .. } => {
                assert_eq!(kind, MsgKind::Action);
                assert_eq!(body.text, "waves");
            }
            other => panic!("expected action message, got {other:?}"),
        }
    }

    #[test]
    fn label_tag_becomes_echo_of() {
        let event = translate_raw("me", "@label=42 :me!u@h PRIVMSG #tirc :hi").unwrap();
        match event {
            ChatEvent::Message { echo_of, .. } => assert_eq!(echo_of, Some(TxnId(42))),
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[test]
    fn join_and_part_become_membership() {
        assert!(matches!(
            translate_raw("me", ":alice!u@h JOIN #tirc").unwrap(),
            ChatEvent::Membership { change: MembershipChange::Join, .. }
        ));
        assert!(matches!(
            translate_raw("me", ":alice!u@h PART #tirc :bye").unwrap(),
            ChatEvent::Membership { change: MembershipChange::Part { .. }, .. }
        ));
    }

    #[test]
    fn names_reply_seeds_roster_with_roles() {
        let events = translate(
            &parse(":irc.example.com 353 me = #tirc :@alice +bob carol"),
            "me",
        );
        assert_eq!(events.len(), 3);

        let roles: Vec<(MemberRole, String)> = events
            .into_iter()
            .map(|event| match event {
                ChatEvent::Membership {
                    who,
                    change: MembershipChange::Present { role },
                    ..
                } => (role, who.id),
                other => panic!("expected present membership, got {other:?}"),
            })
            .collect();

        assert_eq!(roles[0], (MemberRole::Op, "alice".to_string()));
        assert_eq!(roles[1], (MemberRole::Voice, "bob".to_string()));
        assert_eq!(roles[2], (MemberRole::Member, "carol".to_string()));
    }

    #[test]
    fn numeric_reply_keeps_symbolic_code() {
        let event = translate_raw("me", ":irc.example.com 001 me :Welcome").unwrap();
        match event {
            ChatEvent::ServerInfo { code, text, .. } => {
                assert_eq!(code.as_deref(), Some("RPL_WELCOME"));
                assert_eq!(text, "Welcome");
            }
            other => panic!("expected server info, got {other:?}"),
        }
    }
}
