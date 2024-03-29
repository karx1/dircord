use crate::{
    regex, ChannelMappingKey, MembersKey, OptionReplacer, OptionStringKey, RefContentLimitKey,
    SenderKey, UserIdKey,
};
use ellipse::Ellipse;
use fancy_regex::{Captures, Replacer};
use pulldown_cmark::Parser;
use serenity::{
    async_trait,
    client::Context,
    http::CacheHttp,
    model::{
        channel::{Channel, Message, MessageReference, MessageType},
        guild::Member,
        id::GuildId,
        prelude::{ChannelId, GuildMemberUpdateEvent, Ready, Role, RoleId},
        user::User,
    },
    prelude::*,
};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write;

struct StrChunks<'a> {
    v: &'a str,
    size: usize,
}

impl<'a> Iterator for StrChunks<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.v.is_empty() {
            return None;
        }
        if self.v.len() < self.size {
            let res = self.v;
            self.v = &self.v[self.v.len()..];
            return Some(res);
        }

        let mut offset = self.size;

        let res = loop {
            match self.v.get(..offset) {
                Some(v) => break v,
                None => {
                    offset -= 1;
                }
            }
        };

        self.v = &self.v[offset..];

        Some(res)
    }
}

impl<'a> StrChunks<'a> {
    fn new(v: &'a str, size: usize) -> Self {
        Self { v, size }
    }
}

async fn create_prefix(msg: &Message, is_reply: bool, http: impl CacheHttp) -> (String, usize) {
    // it's okay to unwrap here since we know we're in a guild
    let Ok(nick) = msg.member(http).await.map(|m| m.display_name().to_owned()) else { return ("(reply) ".into(), 400 - "(reply) ".len()) };

    let mut chars = nick.char_indices();
    let first_char = chars.next().unwrap().1;
    let second_char_offset = chars.next().unwrap().0;

    let colour_index = (first_char as usize + nick.len()) % 12;

    let prefix = format!(
        "{}<\x03{:02}{}\u{200B}{}\x0F> ",
        if is_reply { "(reply to) " } else { "" },
        colour_index,
        &nick[..second_char_offset],
        &nick[second_char_offset..]
    );
    // this 400 is basically just a guess. we cant send exactly 512 byte messages, because
    // if we do then the server cant send them back without going over the 512 limit itself.
    let content_limit = 400 - prefix.len();

    (prefix, content_limit)
}

pub struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        match msg.kind {
            MessageType::Regular | MessageType::InlineReply => {}
            _ => return,
        }

        let ctx_data = ctx.data.read().await;

        let user_id = ctx_data.get::<UserIdKey>().copied().unwrap();
        let sender = ctx_data.get::<SenderKey>().unwrap();
        let members = ctx_data.get::<MembersKey>().unwrap();
        let raw_prefix = ctx_data
            .get::<OptionStringKey>()
            .unwrap()
            .as_deref()
            .unwrap_or("++");
        let mapping = ctx_data.get::<ChannelMappingKey>().unwrap().clone();
        let ref_content_limit = ctx_data.get::<RefContentLimitKey>().unwrap();

        if user_id == msg.author.id || msg.author.bot {
            return;
        }

        let (prefix, content_limit) = create_prefix(&msg, false, &ctx).await;

        let (channel, channel_id) = match mapping.iter().find(|(_, &v)| v == msg.channel_id.0.get())
        {
            Some((k, v)) => (k.as_str(), ChannelId::from(*v)),
            None => return,
        };

        let attachments: Vec<&str> = msg.attachments.iter().map(|a| a.url.as_str()).collect();

        let roles = channel_id
            .to_channel(&ctx)
            .await
            .unwrap()
            .guild()
            .unwrap()
            .guild_id
            .roles(&ctx)
            .await
            .unwrap();

        let members_lock = members.lock().await;

        let computed = discord_to_irc_processing(&msg.content, &members_lock, &ctx, &roles).await;

        if let Some(MessageReference {
            guild_id,
            channel_id,
            message_id: Some(message_id),
            ..
        }) = msg.message_reference
        {
            if let Ok(mut reply) = channel_id.message(&ctx, message_id).await {
                reply.guild_id = guild_id; // lmao
                let (reply_prefix, reply_content_limit) = create_prefix(&reply, true, &ctx).await;

                let mut content = reply.content;
                content = content.replace("\r\n", " "); // just in case
                content = content.replace('\n', " ");
                let atts: Vec<&str> = reply.attachments.iter().map(|a| &*a.url).collect();
                content = format!("{} {}", content, atts.join(" "));

                content = discord_to_irc_processing(&content, &members_lock, &ctx, &roles).await;

                let to_send = (&*content).truncate_ellipse(
                    ref_content_limit
                        .map(|l| l as usize)
                        .unwrap_or(reply_content_limit),
                );

                sender
                    .send_privmsg(channel, format!("{reply_prefix}{to_send}"))
                    .unwrap();
            }
        }

        if let Some((stripped, false)) = computed
            .strip_prefix(raw_prefix)
            .map(str::trim)
            .map(|v| (v, v.is_empty()))
        {
            let to_send = stripped.trim_matches('\u{f}');
            sender.send_privmsg(channel, &prefix).unwrap();
            sender.send_privmsg(channel, to_send).unwrap();
        } else {
            for line in computed.lines() {
                for chunk in StrChunks::new(line, content_limit) {
                    let to_send = chunk.trim_matches('\u{f}');
                    sender
                        .send_privmsg(channel, &format!("{prefix}{to_send}"))
                        .unwrap();
                }
            }
        }

        for attachment in attachments {
            sender
                .send_privmsg(channel, &format!("{prefix}{attachment}"))
                .unwrap();
        }
    }

    async fn ready(&self, ctx: Context, info: Ready) {
        let id = info.user.id;

        let mut data = ctx.data.write().await;
        data.insert::<UserIdKey>(id);
    }

    async fn guild_member_addition(&self, ctx: Context, new_member: Member) {
        let ctx_data = ctx.data.read().await;
        let mut members = ctx_data.get::<MembersKey>().unwrap().lock().await;
        members.push(new_member);
    }

    async fn guild_member_update(
        &self,
        ctx: Context,
        _: Option<Member>,
        new: Option<Member>,
        _: GuildMemberUpdateEvent,
    ) {
        let ctx_data = ctx.data.read().await;
        let mut members = ctx_data.get::<MembersKey>().unwrap().lock().await;

        if let Some(new) = new {
            let x = members
                .iter()
                .position(|m| m.user.id == new.user.id)
                .unwrap();
            members.remove(x);
            members.push(new);
        }
    }

    async fn guild_member_removal(
        &self,
        ctx: Context,
        _guild_id: GuildId,
        user: User,
        _member: Option<Member>,
    ) {
        let ctx_data = ctx.data.read().await;
        let mut members = ctx_data.get::<MembersKey>().unwrap().lock().await;

        let pos = members.iter().position(|m| m.user.id == user.id).unwrap();
        members.remove(pos);
    }
}

async fn discord_to_irc_processing(
    message: &str,
    members: &[Member],
    ctx: &Context,
    roles: &HashMap<RoleId, Role>,
) -> String {
    struct MemberReplacer<'a> {
        members: &'a [Member],
    }

    impl<'a> Replacer for MemberReplacer<'a> {
        fn replace_append(&mut self, caps: &Captures<'_>, dst: &mut String) {
            let id = caps[1].parse::<u64>().unwrap();

            let display_name = self.members.iter().find_map(|member| {
                (id == member.user.id.0.get()).then(|| member.display_name().to_owned())
            });

            if let Some(display_name) = display_name {
                write!(dst, "@{display_name}").unwrap();
            } else {
                dst.push_str(caps.get(0).unwrap().as_str());
            }
        }
    }

    regex! {
        static PING_RE_1 = r"<@([0-9]+)>";
        static PING_RE_2 = r"<@!([0-9]+)>";
        static PING_RE_3 = r"\{@([0-9]+)\}";
        static EMOJI_RE = r"<:(\w+):[0-9]+>";
        static CHANNEL_RE = r"<#([0-9]+)>";
        static ROLE_RE = r"<@&([0-9]+)>";
        static URL_ESCAPE_RE = r"<(https?://[^\s/$.?#].\S*)>";
    }

    let mut computed = message.to_owned();

    computed = URL_ESCAPE_RE.replace_all(&computed, "$1").into_owned();

    computed = PING_RE_1
        .replace_all(&computed, MemberReplacer { members })
        .into_owned();

    computed = PING_RE_2
        .replace_all(&computed, MemberReplacer { members })
        .into_owned();

    computed = EMOJI_RE.replace_all(&computed, ":$1:").into_owned();

    // FIXME: the await makes it impossible to use `replace_all`, idk how to fix this
    for caps in CHANNEL_RE.captures_iter(&computed.clone()) {
        let replacement = match ChannelId(caps.unwrap()[1].parse().unwrap())
            .to_channel(&ctx)
            .await
        {
            Ok(Channel::Guild(gc)) => Cow::Owned(format!("#{}", gc.name)),
            _ => Cow::Borrowed("#deleted-channel"),
        };

        computed = CHANNEL_RE.replace(&computed, replacement).to_string();
    }

    computed = ROLE_RE
        .replace_all(
            &computed,
            OptionReplacer(|caps: &Captures| {
                roles
                    .get(&RoleId(caps[1].parse().unwrap()))
                    .map(|role| format!("@{}", role.name))
            }),
        )
        .into_owned();

    // switch brackets of unknown pings
    computed = PING_RE_1.replace_all(&computed, "{@$1}").into_owned();

    computed = {
        #[allow(clippy::enum_glob_use)]
        use pulldown_cmark::{Event::*, Tag::*};

        let mut new = String::with_capacity(computed.len());

        let parser = Parser::new(&computed);

        let mut list_level = 0;
        let mut numbered = false;
        let mut next_num = 0;

        for event in parser {
            match event {
                Text(t) | Html(t) => new.push_str(&t),
                Code(t) => write!(new, "`{t}`").unwrap(),
                Start(Emphasis) => new.push('\x1D'),
                Start(Strong) => new.push('\x02'),
                Start(Link(_, _, _)) => {
                    new.push('[');
                }
                End(Link(_, url, title)) => {
                    write!(new, "]: {url}").unwrap();
                    if !title.is_empty() {
                        write!(new, " ({title})").unwrap();
                    }
                }
                Start(List(num)) => {
                    list_level += 1;
                    if let Some(num) = num {
                        numbered = true;
                        next_num = num;
                    } else {
                        numbered = false;
                    }
                }
                End(List(_)) => list_level -= 1,
                Start(Item) => {
                    let prefix = if numbered {
                        format!("{next_num}.")
                    } else {
                        if list_level > 1 { '◦' } else { '•' }.into()
                    };
                    write!(new, "\n{}{} ", "  ".repeat(list_level - 1), prefix).unwrap();
                }
                End(Item) => {
                    if numbered {
                        next_num += 1;
                    }
                }
                Start(BlockQuote) => new.push_str("> "),
                Start(Heading(ty, _, _)) => {
                    write!(new, "{} \x02", "#".repeat(ty as usize)).unwrap();
                }
                SoftBreak | HardBreak | End(Paragraph) => new.push('\n'),
                End(_) => new.push('\x0F'),
                _ => {}
            }
        }

        new
    };

    // switch them back
    computed = PING_RE_3.replace_all(&computed, "<@$1>").into_owned();

    computed
}
