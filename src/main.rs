#![warn(clippy::pedantic)]

mod discord_irc;
mod irc_discord;

use std::{borrow::Cow, collections::HashMap, env, fs::File, io::Read, sync::Arc};

use serenity::{
    http::Http,
    model::{
        guild::Member,
        id::{ChannelId, UserId},
        webhook::Webhook,
    },
    Client as DiscordClient,
};

use tokio::{select, sync::Mutex};

use irc::client::{data::Config, Client as IrcClient, Sender};

use crate::discord_irc::Handler;
use crate::irc_discord::irc_loop;

use fancy_regex::{Captures, Replacer};
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
    let cache = discord_client.cache_and_http.cache.clone();

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
        r = irc_loop(irc_client, http.clone(), cache.clone(), channels.clone(), webhooks_transformed, members) => r.unwrap(),
        r = discord_client.start() => r.unwrap(),
        _ = terminate_signal() => {
            for (_, &v) in channels.iter() {
                let channel_id = ChannelId::from(v);
                channel_id.say(&http, format!("dircord shutting down! (dircord {}-{})", env!("VERGEN_GIT_BRANCH"), &env!("VERGEN_GIT_SHA")[..7])).await.unwrap();
            }
        },
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

async fn parse_webhook_url(http: Arc<Http>, url: String) -> anyhow::Result<Webhook> {
    let url = url.trim_start_matches("https://discord.com/api/webhooks/");
    let split = url.split('/').collect::<Vec<&str>>();
    let id = split[0].parse::<u64>()?;
    let token = split[1].to_string();
    let webhook = http.get_webhook_with_token(id, &token).await?;
    Ok(webhook)
}
