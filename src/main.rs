#![warn(clippy::pedantic)]

use std::{borrow::Cow, collections::HashMap, env, fs::File, io::Read, sync::Arc};

use serenity::{
    async_trait,
    futures::StreamExt,
    http::{Http, CacheHttp},
    model::{
        channel::{Channel, GuildChannel, Message, MessageType, MessageReference},
        guild::{Member, Role},
        id::{ChannelId, GuildId, RoleId, UserId},
        prelude::Ready,
        webhook::Webhook,
    },
    prelude::*,
    Client as DiscordClient,
};

use tokio::{select, sync::Mutex};

use irc::{
    client::{data::Config, Client as IrcClient, Sender},
    proto::Command,
};

use lazy_static::lazy_static;
use regex::{Captures, Regex, Replacer};

use pulldown_cmark::Parser;

use serde::Deserialize;

#[derive(Deserialize)]
struct DircordConfig {
    token: String,
    nickname: Option<String>,
    server: String,
    port: Option<u16>,
    mode: Option<String>,
    tls: Option<bool>,
    raw_prefix: Option<String>,
    channels: HashMap<String, u64>,
    webhooks: Option<HashMap<String, String>>,
}

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

        self.v = &self.v[self.v.len()..];

        Some(res)
    }
}

impl<'a> StrChunks<'a> {
    fn new(v: &'a str, size: usize) -> Self {
        Self { v, size }
    }
}

async fn create_prefix(msg: &Message, is_reply: bool, http: impl CacheHttp) -> (String, usize) {
    let nick = match msg.member(http).await {
        Ok(Member {
            nick: Some(nick), ..
        }) => Cow::Owned(nick),
        _ => Cow::Borrowed(&msg.author.name),
    };

    let first_char = nick.chars().next().unwrap();
    let colour_index = (first_char as usize + nick.len()) % 12;

    let mut first_char = 1;
    loop {
        if nick.get(..first_char).is_some() {
            break;
        }
        first_char += 1;
    }

    let prefix = format!(
        "{}<\x03{:02}{}\u{200B}{}\x0F> ",
        if is_reply { "(reply to) " } else { "" },
        colour_index,
        &nick[..first_char],
        &nick[first_char..]
    );
    let content_limit = 510 - prefix.len();

    (prefix, content_limit)
}

struct Handler;

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

        if user_id == msg.author.id || msg.author.bot {
            return;
        }

        let (prefix, content_limit) = create_prefix(&msg, false, &ctx).await;

        let (channel, channel_id) = match mapping.iter().find(|(_, &v)| v == msg.channel_id.0) {
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

        let computed = discord_to_irc_processing(&msg.content, &**members_lock, &ctx, &roles).await;

        if let Some(MessageReference { guild_id, channel_id, message_id: Some(message_id), .. }) = msg.message_reference {
            if let Ok(mut reply) = channel_id.message(&ctx, message_id).await {
                reply.guild_id = guild_id; // lmao
                let (reply_prefix, reply_content_limit) = create_prefix(&reply, true, &ctx).await;

                let mut content = reply.content;
                content = content.replace("\r\n", " "); // just in case
                content = content.replace('\n', " ");
                content = format!(
                    "{} {}",
                    content,
                    reply
                        .attachments
                        .iter()
                        .map(|a| &*a.url)
                        .collect::<String>()
                );

                let to_send = if content.len() > reply_content_limit {
                    format!("{}...", &content[..reply_content_limit - 3])
                } else {
                    content
                };

                sender
                    .send_privmsg(channel, &format!("{}{}", reply_prefix, to_send))
                    .unwrap();
            }
        }

        if let Some((stripped, false)) = computed
            .strip_prefix(&raw_prefix)
            .map(str::trim)
            .and_then(|v| v.strip_suffix('\x0F'))
            .map(|v| (v, v.is_empty()))
        {
            sender.send_privmsg(channel, &prefix).unwrap();
            sender.send_privmsg(channel, stripped).unwrap();
        } else {
            for line in computed.lines() {
                for chunk in StrChunks::new(line, content_limit) {
                    sender
                        .send_privmsg(channel, &format!("{}{}", prefix, chunk))
                        .unwrap();
                }
            }
        }

        for attachment in attachments {
            sender
                .send_privmsg(channel, &format!("{}{}", prefix, attachment))
                .unwrap();
        }
    }

    async fn ready(&self, ctx: Context, info: Ready) {
        let id = info.user.id;

        let mut data = ctx.data.write().await;
        data.insert::<UserIdKey>(id);
    }

    async fn guild_member_addition(&self, ctx: Context, _: GuildId, new_member: Member) {
        let ctx_data = ctx.data.read().await;
        let mut members = ctx_data.get::<MembersKey>().unwrap().lock().await;
        members.push(new_member);
    }

    async fn guild_member_update(&self, ctx: Context, _: Option<Member>, new: Member) {
        let ctx_data = ctx.data.read().await;
        let mut members = ctx_data.get::<MembersKey>().unwrap().lock().await;

        let x = members
            .iter()
            .position(|m| m.user.id == new.user.id)
            .unwrap();
        members.remove(x);
        members.push(new);
    }
}

macro_rules! type_map_key {
    ($($name:ident => $value:ty),* $(,)?) => {
            $(
            struct $name;

            impl ::serenity::prelude::TypeMapKey for $name {
                type Value = $value;
            }
        )*
    };
}

type_map_key!(
    HttpKey => Arc<Http>,
    ChannelIdKey => ChannelId,
    UserIdKey => UserId,
    SenderKey => Sender,
    MembersKey => Arc<Mutex<Vec<Member>>>,
    StringKey => String,
    OptionStringKey => Option<String>,
    ChannelMappingKey => HashMap<String, u64>,
);

#[cfg(unix)]
async fn terminate_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).unwrap();
    let mut sigint = signal(SignalKind::interrupt()).unwrap();

    select! {
        _ = sigterm.recv() => {},
        _ = sigint.recv() => {},
    }
}

#[cfg(windows)]
async fn terminate_signal() {
    use tokio::signal::windows::ctrl_c;
    let mut ctrlc = ctrl_c().unwrap();
    let _ = ctrlc.recv().await;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filename = env::args()
        .nth(1)
        .map_or(Cow::Borrowed("config.toml"), Cow::Owned);
    let mut data = String::new();
    File::open(&*filename)?.read_to_string(&mut data)?;

    let conf: DircordConfig = toml::from_str(&data)?;

    let mut discord_client = DiscordClient::builder(&conf.token)
        .event_handler(Handler)
        .await?;

    let config = Config {
        nickname: conf.nickname,
        server: Some(conf.server),
        port: conf.port,
        channels: conf.channels.keys().map(Clone::clone).collect(),
        use_tls: conf.tls,
        umodes: conf.mode,
        ..Config::default()
    };

    let irc_client = IrcClient::from_config(config).await?;

    let http = discord_client.cache_and_http.http.clone();

    let members = Arc::new(Mutex::new({
        let channel_id = ChannelId::from(*conf.channels.iter().next().unwrap().1);

        channel_id
            .to_channel(discord_client.cache_and_http.clone())
            .await?
            .guild()
            .unwrap() // we can panic here because if it's not a guild channel then the bot shouldn't even work
            .guild_id
            .members(&http, None, None)
            .await?
    }));

    let channels = Arc::new(conf.channels);

    {
        let mut data = discord_client.data.write().await;
        data.insert::<SenderKey>(irc_client.sender());
        data.insert::<MembersKey>(members.clone());
        data.insert::<OptionStringKey>(conf.raw_prefix);
        data.insert::<ChannelMappingKey>((*channels).clone());
    }

    let mut webhooks_transformed: HashMap<String, Webhook> = HashMap::new();

    if let Some(webhooks) = conf.webhooks {
        for (channel, wh) in webhooks {
            let parsed = parse_webhook_url(http.clone(), wh)
                .await
                .expect("Invalid webhook URL");

            webhooks_transformed.insert(channel.clone(), parsed);
        }
    }

    select! {
        r = irc_loop(irc_client, http.clone(), channels.clone(), webhooks_transformed, members) => r?,
        r = discord_client.start() => r?,
        _ = terminate_signal() => {
            for (_, &v) in channels.iter() {
                let channel_id = ChannelId::from(v);
                channel_id.say(&http, format!("dircord shutting down! (dircord {}-{})", env!("VERGEN_GIT_BRANCH"), &env!("VERGEN_GIT_SHA")[..7])).await?;
            }
        },
    }

    Ok(())
}

macro_rules! unwrap_or_continue {
    ($opt:expr) => {
        match $opt {
            ::core::option::Option::Some(v) => v,
            ::core::option::Option::None => continue,
        }
    };
}

async fn irc_loop(
    mut client: IrcClient,
    http: Arc<Http>,
    mapping: Arc<HashMap<String, u64>>,
    webhooks: HashMap<String, Webhook>,
    members: Arc<Mutex<Vec<Member>>>,
) -> anyhow::Result<()> {
    let mut avatar_cache: HashMap<String, Option<String>> = HashMap::new();
    let mut id_cache: HashMap<String, Option<u64>> = HashMap::new();
    let mut channel_users: HashMap<String, Vec<String>> = HashMap::new();

    client.identify()?;
    let mut stream = client.stream()?;

    for k in mapping.keys() {
        client.send(Command::NAMES(Some(k.clone()), None))?;
    }

    while let Some(orig_message) = stream.next().await.transpose()? {
        let nickname = unwrap_or_continue!(orig_message.source_nickname());

        if let Command::PRIVMSG(ref channel, ref message) = orig_message.command {
            let channel_id = ChannelId::from(*unwrap_or_continue!(mapping.get(channel)));

            let channels = channel_id
                .to_channel(&http)
                .await?
                .guild()
                .unwrap()
                .guild_id
                .channels(&http)
                .await?;

            let members_lock = members.lock().await;

            let computed =
                irc_to_discord_processing(message, &*members_lock, &mut id_cache, &channels);

            if let Some(webhook) = webhooks.get(channel) {
                let avatar = &*avatar_cache.entry(nickname.to_owned()).or_insert_with(|| {
                    members_lock.iter().find_map(|member| {
                        (*member.display_name() == nickname)
                            .then(|| member.user.avatar_url())
                            .flatten()
                    })
                });

                webhook
                    .execute(&http, false, |w| {
                        if let Some(ref url) = avatar {
                            w.avatar_url(url);
                        }

                        w.username(nickname).content(computed)
                    })
                    .await?;
            } else {
                channel_id
                    .say(&http, format!("<{}> {}", nickname, computed))
                    .await?;
            }
        } else if let Command::JOIN(ref channel, _, _) = orig_message.command {
            let channel_id = ChannelId::from(*unwrap_or_continue!(mapping.get(channel)));
            let users = unwrap_or_continue!(channel_users.get_mut(channel));

            users.push(nickname.to_string());

            channel_id
                .say(&http, format!("*{}* has joined the channel", nickname))
                .await?;
        } else if let Command::PART(ref channel, ref reason) = orig_message.command {
            let users = unwrap_or_continue!(channel_users.get_mut(channel));
            let channel_id = ChannelId::from(*unwrap_or_continue!(mapping.get(channel)));
            let pos = unwrap_or_continue!(users.iter().position(|u| u == nickname));

            users.swap_remove(pos);

            let reason = reason.as_deref().unwrap_or("Connection closed");

            channel_id
                .say(&http, format!("*{}* has quit ({})", nickname, reason))
                .await?;
        } else if let Command::QUIT(ref reason) = orig_message.command {
            for (channel, users) in &mut channel_users {
                let channel_id = ChannelId::from(*unwrap_or_continue!(mapping.get(channel)));
                let pos = unwrap_or_continue!(users.iter().position(|u| u == nickname));

                users.swap_remove(pos);

                let reason = reason.as_deref().unwrap_or("Connection closed");

                channel_id
                    .say(&http, format!("*{}* has quit ({})", nickname, reason))
                    .await?;
            }
        } else if let Command::Response(ref response, ref args) = orig_message.command {
            use irc::client::prelude::Response;

            if let Response::RPL_NAMREPLY = response {
                let channel = args[2].to_string();
                let users = args[3]
                    .split(' ')
                    .map(ToOwned::to_owned)
                    .collect::<Vec<String>>();

                channel_users.insert(channel, users);
            }
        } else if let Command::NICK(ref new_nick) = orig_message.command {
            for (channel, users) in &mut channel_users {
                let channel_id = ChannelId::from(*unwrap_or_continue!(mapping.get(channel)));
                let pos = unwrap_or_continue!(users.iter().position(|u| u == nickname));

                users[pos] = new_nick.to_string();

                channel_id
                    .say(
                        &http,
                        format!("*{}* is now known as *{}*", nickname, new_nick),
                    )
                    .await?;
            }
        }
    }
    Ok(())
}

struct OptionReplacer<F>(F);

impl<T: AsRef<str>, F: for<'r, 't> FnMut(&'r Captures<'t>) -> Option<T>> Replacer
    for OptionReplacer<F>
{
    fn replace_append(&mut self, caps: &Captures<'_>, dst: &mut String) {
        match (self.0)(caps) {
            Some(v) => dst.push_str(v.as_ref()),
            None => dst.push_str(caps.get(0).unwrap().as_str()),
        }
    }
}

fn irc_to_discord_processing(
    message: &str,
    members: &[Member],
    id_cache: &mut HashMap<String, Option<u64>>,
    channels: &HashMap<ChannelId, GuildChannel>,
) -> String {
    struct MemberReplacer<'a> {
        id_cache: &'a mut HashMap<String, Option<u64>>,
        members: &'a [Member],
        formatter: fn(u64) -> String,
    }

    impl<'a> Replacer for MemberReplacer<'a> {
        fn replace_append(&mut self, caps: &Captures<'_>, dst: &mut String) {
            let slice = &caps[1];

            let id = *self.id_cache.entry(slice.to_owned()).or_insert_with(|| {
                self.members.iter().find_map(|member| {
                    (slice == member.display_name().as_str()).then(|| member.user.id.0)
                })
            });

            if let Some(id) = id {
                dst.push_str(&(self.formatter)(id));
            } else {
                dst.push_str(caps.get(0).unwrap().as_str());
            }
        }
    }

    lazy_static! {
        static ref PING_NICK_1: Regex = Regex::new(r"^([\w+]+)(?::|,)").unwrap();
        static ref PING_RE_2: Regex = Regex::new(r"^@([\w\S]+)").unwrap();
        static ref PING_RE_3: Regex = Regex::new(r"\b@([\w\S]+)").unwrap();
        static ref CONTROL_CHAR_RE: Regex =
            Regex::new(r"\x1f|\x02|\x12|\x0f|\x16|\x03(?:\d{1,2}(?:,\d{1,2})?)?").unwrap();
        static ref WHITESPACE_RE: Regex = Regex::new(r"^\s").unwrap();
        static ref CHANNEL_RE: Regex = Regex::new(r"#([A-Za-z-*]+)").unwrap();
    }

    let mut computed = message.to_owned();

    let mut is_code = false;
    if WHITESPACE_RE.is_match(message) && !PING_RE_3.is_match(message) {
        computed = format!("`{}`", computed);
        is_code = true;
    }

    if !is_code {
        PING_RE_3.replace_all(
            &computed,
            MemberReplacer {
                id_cache,
                members,
                formatter: |v| format!("<@{}>", v),
            },
        );

        PING_RE_3.replace_all(
            &computed,
            MemberReplacer {
                id_cache,
                members,
                formatter: |v| format!("<@{}>", v),
            },
        );

        PING_NICK_1.replace_all(
            &computed,
            MemberReplacer {
                id_cache,
                members,
                formatter: |v| format!("<@{}>", v),
            },
        );

        CHANNEL_RE.replace_all(
            &computed,
            OptionReplacer(|caps: &Captures| {
                channels
                    .iter()
                    .find_map(|(id, c)| (c.name == caps[1]).then(|| format!("<#{}>", id.0)))
            }),
        );

        let mut has_opened_bold = false;
        let mut has_opened_italic = false;

        for c in computed.clone().chars() {
            if c == '\x02' {
                computed = computed.replacen('\x02', "**", 1);
                has_opened_bold = true;
            }

            if c == '\x1D' {
                computed = computed.replacen('\x1D', "*", 1);
                has_opened_italic = true;
            }

            if c == '\x0F' {
                if has_opened_bold {
                    computed = computed.replacen('\x0F', "**", 1);
                    has_opened_bold = false;
                } else if has_opened_italic {
                    computed = computed.replacen('\x0F', "*", 1);
                    has_opened_italic = false;
                }
            }
        }

        if has_opened_italic {
            computed.push('*');
        }

        if has_opened_bold {
            computed.push_str("**");
        }

        computed = CONTROL_CHAR_RE.replace_all(message, "").to_string();
    }

    computed
}

async fn discord_to_irc_processing(
    message: &str,
    members: &[Member],
    ctx: &Context,
    roles: &HashMap<RoleId, Role>,
) -> String {
    struct MemberReplacer<'a> {
        members: &'a [Member],
        formatter: fn(String) -> String,
    }

    impl<'a> Replacer for MemberReplacer<'a> {
        fn replace_append(&mut self, caps: &Captures<'_>, dst: &mut String) {
            let id = caps[1].parse::<u64>().unwrap();

            let display_name = self.members.iter().find_map(|member| {
                (id == member.user.id.0).then(|| member.display_name().into_owned())
            });

            if let Some(display_name) = display_name {
                dst.push_str(&(self.formatter)(display_name));
            } else {
                dst.push_str(caps.get(0).unwrap().as_str());
            }
        }
    }

    lazy_static! {
        static ref PING_RE_1: Regex = Regex::new(r"<@([0-9]+)>").unwrap();
        static ref PING_RE_2: Regex = Regex::new(r"<@!([0-9]+)>").unwrap();
        static ref EMOJI_RE: Regex = Regex::new(r"<:(\w+):[0-9]+>").unwrap();
        static ref CHANNEL_RE: Regex = Regex::new(r"<#([0-9]+)>").unwrap();
        static ref ROLE_RE: Regex = Regex::new(r"<@&([0-9]+)>").unwrap();
    }

    let mut computed = message.to_owned();

    computed = PING_RE_1
        .replace_all(
            &computed,
            MemberReplacer {
                members,
                formatter: |v| format!("@{}", v),
            },
        )
        .into_owned();

    computed = PING_RE_2
        .replace_all(
            &computed,
            MemberReplacer {
                members,
                formatter: |v| format!("@{}", v),
            },
        )
        .into_owned();

    computed = EMOJI_RE.replace_all(&computed, "$1").into_owned();

    // FIXME: the await makes it impossible to use `replace_all`, idk how to fix this
    for caps in CHANNEL_RE.captures_iter(&computed.clone()) {
        let replacement = match ChannelId(caps[1].parse().unwrap()).to_channel(&ctx).await {
            Ok(Channel::Guild(gc)) => Cow::Owned(format!("#{}", gc.name)),
            Ok(Channel::Category(cat)) => Cow::Owned(format!("#{}", cat.name)),
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

    computed = {
        #[allow(clippy::enum_glob_use)]
        use pulldown_cmark::{Tag::*, Event::*};

        let mut new = String::with_capacity(computed.len());

        for line in computed.lines() {
            let parser = Parser::new(line);

            let mut computed_line = String::with_capacity(line.len());

            for event in parser {
                match event {
                    Text(t) | Html(t) => computed_line.push_str(&t),
                    Code(t) => computed_line.push_str(&format!("`{}`", t)),
                    End(_) => computed_line.push('\x0F'),
                    Start(Emphasis) => computed_line.push('\x1D'),
                    Start(Strong) => computed_line.push('\x02'),
                    Start(Link(_, dest, _)) => {
                        computed_line.push_str(&dest);
                        continue;
                    }
                    Start(List(num)) => {
                        if let Some(num) = num {
                            computed_line.push_str(&format!("{}. ", num));
                        } else {
                            computed_line.push_str("- ");
                        }
                    }
                    Start(BlockQuote) => computed_line.push_str("> "),
                    _ => {}
                }
            }

            computed_line.push('\n');

            new.push_str(&computed_line);
        }

        new
    };

    computed
}

async fn parse_webhook_url(http: Arc<Http>, url: String) -> anyhow::Result<Webhook> {
    let url = url.trim_start_matches("https://discord.com/api/webhooks/");
    let split = url.split('/').collect::<Vec<&str>>();
    let id = split[0].parse::<u64>()?;
    let token = split[1].to_string();
    let webhook = http.get_webhook_with_token(id, &token).await?;
    Ok(webhook)
}
