#![warn(clippy::pedantic)]

mod discord_irc;
mod irc_discord;

use std::{borrow::Cow, collections::HashMap, env, fs::File, io::Read, sync::Arc};

use serenity::{
    async_trait,
    futures::StreamExt,
    http::{CacheHttp, Http},
    model::{
        channel::{Channel, GuildChannel, Message, MessageReference, MessageType},
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

use fancy_regex::{Captures, Replacer};

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

pub(crate) struct StrChunks<'a> {
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

#[macro_export]
macro_rules! regex {
    ($(static $name:ident = $regex:literal;)*) => {
        ::lazy_static::lazy_static! {
            $(
                static ref $name: ::fancy_regex::Regex = ::fancy_regex::Regex::new($regex).unwrap();
            )*
        }
    };
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
    }

    impl<'a> Replacer for MemberReplacer<'a> {
        fn replace_append(&mut self, caps: &Captures<'_>, dst: &mut String) {
            let slice = &caps[1];

            let id = self
                .id_cache
                .entry(slice.to_owned())
                .or_insert_with(|| {
                    self.members.iter().find_map(|member| {
                        (slice == member.display_name().as_str()).then(|| member.user.id.0)
                    })
                })
                .map(UserId);

            if let Some(id) = id {
                dst.push_str(&id.mention().to_string());
            } else {
                dst.push_str(caps.get(0).unwrap().as_str());
            }
        }
    }

    regex! {
        static PING_NICK_1 = r"^([\w+]+)(?::|,)";
        static PING_RE_2 = r"(?<=\s|^)@([\w\S]+)";
        static CONTROL_CHAR_RE = r"\x1f|\x02|\x12|\x0f|\x16|\x03(?:\d{1,2}(?:,\d{1,2})?)?";
        static WHITESPACE_RE = r"^\s";
        static CHANNEL_RE = r"#([A-Za-z-*]+)";
    }

    if WHITESPACE_RE.is_match(message).unwrap() && !PING_RE_2.is_match(message).unwrap() {
        return format!("`{}`", message);
    }

    let mut computed = message.to_owned();

    computed = PING_NICK_1
        .replace_all(&computed, MemberReplacer { id_cache, members })
        .into_owned();

    computed = PING_RE_2
        .replace_all(&computed, MemberReplacer { id_cache, members })
        .into_owned();

    computed = CHANNEL_RE
        .replace_all(
            &computed,
            OptionReplacer(|caps: &Captures| {
                channels
                    .iter()
                    .find_map(|(id, c)| (c.name == caps[1]).then(|| format!("<#{}>", id.0)))
            }),
        )
        .into_owned();

    computed = {
        let mut new = String::with_capacity(computed.len());
        let mut has_opened_bold = false;
        let mut has_opened_italic = false;

        for c in computed.chars() {
            if c == '\x02' || (c == '\x0F' && has_opened_bold) {
                new.push_str("**");
                has_opened_bold = !has_opened_bold;
            } else if c == '\x1D' || (c == '\x0F' && has_opened_italic) {
                new.push('*');
                has_opened_italic = !has_opened_italic;
            } else {
                new.push(c);
            }
        }

        if has_opened_italic {
            new.push('*');
        }

        if has_opened_bold {
            new.push_str("**");
        }

        CONTROL_CHAR_RE.replace_all(&new, "").into_owned()
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
