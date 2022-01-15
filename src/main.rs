use std::{collections::HashMap, env, fs::File, io::Read, sync::Arc};

use serenity::{
    async_trait,
    futures::StreamExt,
    http::Http,
    model::{
        channel::Message,
        guild::Member,
        id::{ChannelId, UserId},
        prelude::Ready,
        webhook::Webhook,
    },
    prelude::*,
    Client as DiscordClient,
};

use irc::{
    client::{data::Config, Client as IrcClient, Sender},
    proto::Command,
};

use lazy_static::lazy_static;
use regex::Regex;

use serde::Deserialize;

#[derive(Deserialize)]
struct DircordConfig {
    token: String,
    webhook: Option<String>,
    nickname: Option<String>,
    server: String,
    port: Option<u16>,
    channels: Vec<String>,
    mode: Option<String>,
    tls: Option<bool>,
}

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        let nick = {
            if let Some(member) = msg.member {
                match member.nick {
                    Some(n) => n,
                    None => msg.author.name,
                }
            } else {
                msg.author.name
            }
        };

        let byte = nick.chars().nth(0).unwrap() as u8;

        let colour_index = (byte as usize + nick.len()) % 12;
        let formatted = format!("\x03{:02}", colour_index);

        let mut new_nick = String::with_capacity(nick.len());
        new_nick.push_str(&formatted);

        for char in nick.chars() {
            new_nick.push(char);
            new_nick.push('\u{200B}');
            new_nick.push_str(&formatted);
        }
        new_nick.push_str("\x030");

        let (user_id, sender, members) = {
            let data = ctx.data.read().await;

            let user_id = data.get::<UserIdKey>().unwrap().to_owned();
            let sender = data.get::<SenderKey>().unwrap().to_owned();
            let members = data.get::<MembersKey>().unwrap().to_owned();

            (user_id, sender, members)
        };

        let attachments: Vec<String> = msg.attachments.iter().map(|a| a.url.clone()).collect();

        lazy_static! {
            static ref PING_RE_1: Regex = Regex::new(r"<@[0-9]+>").unwrap();
            static ref PING_RE_2: Regex = Regex::new(r"<@![0-9]+>").unwrap();
        }

        let mut id_cache: HashMap<u64, String> = HashMap::new();

        if PING_RE_1.is_match(&msg.content) {
            for mat in PING_RE_1.find_iter(&msg.content) {
                let slice = &msg.content[mat.start() + 2..mat.end()-1];
                let id = slice.parse::<u64>().unwrap();
                for member in &*members {
                    if id == member.user.id.0 {
                        let nick = {
                            match &member.nick {
                                Some(n) => n.clone(),
                                None => member.user.name.clone()
                            }
                        };

                        id_cache.insert(id, nick);
                    }
                }
            }
        } else if PING_RE_2.is_match(&msg.content) {
            for mat in PING_RE_2.find_iter(&msg.content) {
                let slice = &msg.content[mat.start() + 3..mat.end()-1];
                let id = slice.parse::<u64>().unwrap();
                for member in &*members {
                    if id == member.user.id.0 {
                        let nick = {
                            match &member.nick {
                                Some(n) => n.clone(),
                                None => member.user.name.clone()
                            } 
                        };

                        id_cache.insert(id, nick);
                    }
                }
            }
        }

        let mut computed = msg.content.clone();

        for mat in PING_RE_1.find_iter(&msg.content) {
            let slice = &msg.content[mat.start() + 2..mat.end()-1];
            let id = slice.parse::<u64>().unwrap();
            if let Some(cached) = id_cache.get(&id) {
                computed = PING_RE_1.replace(&computed, format!("@{}", cached)).to_string();
            }
        }

        for mat in PING_RE_2.find_iter(&msg.content) {
            let slice = &msg.content[mat.start() + 3..mat.end()-1];
            let id = slice.parse::<u64>().unwrap();
            if let Some(cached) = id_cache.get(&id) {
                computed = PING_RE_2.replace(&computed, format!("@{}", cached)).to_string();
            }
        }

        if user_id != msg.author.id && !msg.author.bot {
            send_irc_message(&sender, &format!("<{}> {}", new_nick, computed))
                .await
                .unwrap();
            for attachment in attachments {
                send_irc_message(&sender, &format!("<{}> {}", new_nick, attachment))
                    .await
                    .unwrap();
            }
        }
    }

    async fn ready(&self, ctx: Context, info: Ready) {
        let id = info.user.id;

        let mut data = ctx.data.write().await;
        data.insert::<UserIdKey>(id);
    }
}

struct HttpKey;
struct ChannelIdKey;
struct UserIdKey;
struct SenderKey;
struct MembersKey;

impl TypeMapKey for HttpKey {
    type Value = Arc<Http>;
}

impl TypeMapKey for ChannelIdKey {
    type Value = ChannelId;
}

impl TypeMapKey for UserIdKey {
    type Value = UserId;
}

impl TypeMapKey for SenderKey {
    type Value = Sender;
}

impl TypeMapKey for MembersKey {
    type Value = Arc<Vec<Member>>;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filename = env::args().nth(1).unwrap_or(String::from("config.toml"));
    let mut data = String::new();
    File::open(filename)?.read_to_string(&mut data)?;

    let conf: DircordConfig = toml::from_str(&data)?;

    let mut discord_client = DiscordClient::builder(&conf.token)
        .event_handler(Handler)
        .await?;

    let channel_id = ChannelId(831255708875751477);

    let config = Config {
        nickname: conf.nickname,
        server: Some(conf.server),
        port: conf.port,
        channels: conf.channels,
        use_tls: conf.tls,
        umodes: conf.mode,
        ..Config::default()
    };

    let irc_client = IrcClient::from_config(config).await?;

    let http = discord_client.cache_and_http.http.clone();

    let members = Arc::new(channel_id
        .to_channel(discord_client.cache_and_http.clone())
        .await?
        .guild()
        .unwrap() // we can panic here because if it's not a guild channel then the bot shouldn't even work
        .guild_id
        .members(&http, None, None)
        .await?);

    {
        let mut data = discord_client.data.write().await;
        data.insert::<SenderKey>(irc_client.sender());
        data.insert::<MembersKey>(members.clone());
    }

    let webhook = parse_webhook_url(http.clone(), conf.webhook)
        .await
        .expect("Invalid webhook URL");

    tokio::spawn(async move {
        irc_loop(irc_client, http, channel_id, webhook, members)
            .await
            .unwrap();
    });
    discord_client.start().await?;

    Ok(())
}

async fn send_irc_message(sender: &Sender, content: &str) -> anyhow::Result<()> {
    sender.send_privmsg("#no-normies", content)?;
    Ok(())
}

async fn irc_loop(
    mut client: IrcClient,
    http: Arc<Http>,
    channel_id: ChannelId,
    webhook: Option<Webhook>,
    members: Arc<Vec<Member>>,
) -> anyhow::Result<()> {
    let mut avatar_cache: HashMap<String, Option<String>> = HashMap::new();
    let mut id_cache: HashMap<String, Option<u64>> = HashMap::new();

    lazy_static! {
        static ref PING_NICK_1: Regex = Regex::new(r"^[\w+]+(:|,)").unwrap();
        static ref PING_RE_2: Regex = Regex::new(r"@[^0-9\s]+").unwrap();
    }

    client.identify()?;
    let mut stream = client.stream()?;

    while let Some(orig_message) = stream.next().await.transpose()? {
        print!("{}", orig_message);
        if let Command::PRIVMSG(_, ref message) = orig_message.command {
            let nickname = orig_message.source_nickname().unwrap();
            let mut mentioned_1: Option<u64> = None;
            if PING_NICK_1.is_match(message) {
                if let Some(mat) = PING_NICK_1.find(message) {
                    let slice = &message[mat.start()..mat.end() - 1];
                    if let Some(id) = id_cache.get(slice) {
                        mentioned_1 = id.to_owned();
                    } else {
                        let mut found = false;
                        for member in &*members {
                            let nick = match &member.nick {
                                Some(s) => s.to_owned(),
                                None => member.user.name.clone(),
                            };

                            if nick == slice {
                                found = true;
                                let id = member.user.id.0;
                                mentioned_1 = Some(id);
                            }
                        }

                        if !found {
                            mentioned_1 = None;
                        }
                    }
                }
            }
            if PING_RE_2.is_match(message) {
                for mat in PING_RE_2.find_iter(message) {
                    let slice = &message[mat.start() + 1..mat.end()];
                    dbg!(slice);
                    if id_cache.get(slice).is_none() {
                        let mut found = false;
                        for member in &*members {
                            let nick = match &member.nick {
                                Some(s) => s.to_owned(),
                                None => member.user.name.clone(),
                            };

                            if nick == slice {
                                found = true;
                                let id = member.user.id.0;
                                id_cache.insert(slice.to_string(), Some(id));
                                break;
                            }
                        }

                        if !found {
                            id_cache.insert(slice.to_string(), None);
                        }
                    }
                }
            }
            if let Some(ref webhook) = webhook {
                if avatar_cache.get(nickname).is_none() {
                    let mut found = false;
                    for member in &*members {
                        let nick = match &member.nick {
                            Some(s) => s.to_owned(),
                            None => member.user.name.clone(),
                        };

                        if nick == nickname {
                            found = true;
                            let avatar_url = member.user.avatar_url();
                            avatar_cache.insert(nickname.to_string(), avatar_url);
                            break;
                        }
                    }

                    if !found {
                        avatar_cache.insert(nickname.to_string(), None); // user is not in the guild
                    }
                }

                webhook
                    .execute(&http, false, |w| {
                        if let Some(cached) = avatar_cache.get(nickname) {
                            if let &Some(ref url) = cached {
                                w.avatar_url(url);
                            }
                        }

                        let mut computed = message.to_string();

                        for mat in PING_RE_2.find_iter(message) {
                            let slice = &message[mat.start() + 1..mat.end()];
                            if let Some(cached) = id_cache.get(slice) {
                                if let &Some(id) = cached {
                                    computed = PING_RE_2
                                        .replace(&computed, format!("<@{}>", id))
                                        .to_string();
                                }
                            }
                        }

                        if let Some(id) = mentioned_1 {
                            computed = PING_NICK_1
                                .replace(&computed, format!("<@{}>", id))
                                .to_string();
                        }

                        w.username(nickname);
                        w.content(computed);
                        w
                    })
                    .await?;
            } else {
                let mut computed = message.to_string();

                for mat in PING_RE_2.find_iter(message) {
                    let slice = &message[mat.start() + 1..mat.end()];
                    if let Some(cached) = id_cache.get(slice) {
                        if let &Some(id) = cached {
                            computed = PING_RE_2
                                .replace(&computed, format!("<@{}>", id))
                                .to_string();
                        }
                    }
                }

                if let Some(id) = mentioned_1 {
                    computed = PING_NICK_1
                        .replace(&computed, format!("<@{}>", id))
                        .to_string();
                }

                channel_id
                    .say(&http, format!("<{}> {}", nickname, computed))
                    .await?;
            }
        } else if let Command::JOIN(_, _, _) = orig_message.command {
            let nickname = orig_message.source_nickname().unwrap();
            channel_id
                .say(&http, format!("*{}* has joined the channel", nickname))
                .await?;
        } else if let Command::PART(_, ref reason) | Command::QUIT(ref reason) =
            orig_message.command
        {
            let nickname = orig_message.source_nickname().unwrap();
            let reason = reason
                .as_ref()
                .unwrap_or(&String::from("Connection closed"))
                .to_string();
            channel_id
                .say(&http, format!("*{}* has quit ({})", nickname, reason))
                .await?;
        }
    }
    Ok(())
}

async fn parse_webhook_url(
    http: Arc<Http>,
    url: Option<String>,
) -> anyhow::Result<Option<Webhook>> {
    if let Some(url) = url {
        let url = url.trim_start_matches("https://discord.com/api/webhooks/");
        let split = url.split("/").collect::<Vec<&str>>();
        let id = split[0].parse::<u64>()?;
        let token = split[1].to_string();
        let webhook = http.get_webhook_with_token(id, &token).await?;
        Ok(Some(webhook))
    } else {
        Ok(None)
    }
}
