use std::{collections::HashMap, env, fs::File, io::Read, sync::Arc};

use serenity::{
    async_trait,
    futures::StreamExt,
    http::Http,
    model::{
        channel::Message,
        guild::Member,
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
use regex::Regex;

use pulldown_cmark::Parser;

use serde::Deserialize;

#[derive(Deserialize)]
struct DircordConfig {
    token: String,
    webhook: Option<String>,
    nickname: Option<String>,
    server: String,
    port: Option<u16>,
    channel: String,
    mode: Option<String>,
    tls: Option<bool>,
    channel_id: u64,
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

        let mut new_nick = String::with_capacity(formatted.len() + nick.len() + 2);

        new_nick.push_str(&formatted);
        new_nick.push_str(&nick);
        new_nick.push('\x0F');

        new_nick = format!(
            "{}\u{200B}{}",
            &new_nick[..formatted.len() + 1],
            &new_nick[formatted.len() + 1..]
        );

        let nick_bytes = new_nick.len() + 3; // +1 for the space and +2 for the <>
        let content_limit = 510 - nick_bytes;

        let (user_id, sender, members, channel_id, channel) = {
            let data = ctx.data.read().await;

            let user_id = data.get::<UserIdKey>().unwrap().to_owned();
            let sender = data.get::<SenderKey>().unwrap().to_owned();
            let members = data.get::<MembersKey>().unwrap().to_owned();
            let channel_id = data.get::<ChannelIdKey>().unwrap().to_owned();
            let channel = data.get::<StringKey>().unwrap().to_owned();

            (user_id, sender, members, channel_id, channel)
        };

        let attachments: Vec<String> = msg.attachments.iter().map(|a| a.url.clone()).collect();

        if msg.content.starts_with("```") {
            let mut lines = msg.content.split("\n").collect::<Vec<&str>>();
            // remove the backticks
            lines.remove(lines.len() - 1);
            lines.remove(0);

            if user_id != msg.author.id && !msg.author.bot && msg.channel_id == channel_id {
                for line in lines {
                    send_irc_message(&sender, &channel, &format!("<{}> {}", new_nick, line))
                        .await
                        .unwrap();
                }

                for attachment in attachments {
                    send_irc_message(&sender, &channel, &format!("<{}> {}", new_nick, attachment))
                        .await
                        .unwrap();
                }
            }

            return;
        }

        lazy_static! {
            static ref PING_RE_1: Regex = Regex::new(r"<@[0-9]+>").unwrap();
            static ref PING_RE_2: Regex = Regex::new(r"<@![0-9]+>").unwrap();
            static ref EMOJI_RE: Regex = Regex::new(r"<:\w+:[0-9]+>").unwrap();
            static ref CHANNEL_RE: Regex = Regex::new(r"<#[0-9]+>").unwrap();
            static ref ROLE_RE: Regex = Regex::new(r"<@&[0-9]+>").unwrap();
        }

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

        let mut id_cache: HashMap<u64, String> = HashMap::new();

        if PING_RE_1.is_match(&msg.content) {
            for mat in PING_RE_1.find_iter(&msg.content) {
                let slice = &msg.content[mat.start() + 2..mat.end() - 1];
                let id = slice.parse::<u64>().unwrap();
                for member in &*members.lock().await {
                    if id == member.user.id.0 {
                        let nick = {
                            match &member.nick {
                                Some(n) => n.clone(),
                                None => member.user.name.clone(),
                            }
                        };

                        id_cache.insert(id, nick);
                    }
                }
            }
        } else if PING_RE_2.is_match(&msg.content) {
            for mat in PING_RE_2.find_iter(&msg.content) {
                let slice = &msg.content[mat.start() + 3..mat.end() - 1];
                let id = slice.parse::<u64>().unwrap();
                for member in &*members.lock().await {
                    if id == member.user.id.0 {
                        let nick = {
                            match &member.nick {
                                Some(n) => n.clone(),
                                None => member.user.name.clone(),
                            }
                        };

                        id_cache.insert(id, nick);
                    }
                }
            }
        }

        let mut computed = String::new();
        let mut replaced = msg.content.clone();

        for mat in PING_RE_1.find_iter(&msg.content) {
            let slice = &msg.content[mat.start() + 2..mat.end() - 1];
            let id = slice.parse::<u64>().unwrap();
            if let Some(cached) = id_cache.get(&id) {
                replaced = PING_RE_1
                    .replace(&replaced, format!("@{}", cached))
                    .to_string();
            }
        }

        for mat in PING_RE_2.find_iter(&msg.content) {
            let slice = &msg.content[mat.start() + 3..mat.end() - 1];
            let id = slice.parse::<u64>().unwrap();
            if let Some(cached) = id_cache.get(&id) {
                replaced = PING_RE_2
                    .replace(&replaced, format!("@{}", cached))
                    .to_string();
            }
        }

        for mat in EMOJI_RE.find_iter(&msg.content) {
            let slice = &msg.content[mat.start()..mat.end()];

            let parts = slice.split(':').collect::<Vec<&str>>();

            let formatted = format!(":{}:", parts[1]); // ignore the opening bracket in [0]

            replaced = EMOJI_RE.replace(&replaced, formatted).to_string();
        }

        for mat in CHANNEL_RE.find_iter(&msg.content) {
            use serenity::model::channel::Channel::*;

            let slice = &msg.content[mat.start() + 2..mat.end() - 1];
            let parsed: u64 = slice.parse().unwrap();

            let mentioned_channel_id = ChannelId(parsed);

            if let Ok(chan) = mentioned_channel_id.to_channel(&ctx).await {
                match chan {
                    Private(_) => {
                        replaced = CHANNEL_RE
                            .replace(&replaced, "#invalid-channel")
                            .to_string()
                    }
                    Guild(gc) => {
                        replaced = CHANNEL_RE
                            .replace(&replaced, format!("#{}", gc.name))
                            .to_string()
                    }
                    Category(cat) => {
                        replaced = CHANNEL_RE
                            .replace(&replaced, format!("#{}", cat.name))
                            .to_string()
                    }
                    _ => {
                        replaced = CHANNEL_RE
                            .replace(&replaced, "#invalid-channel")
                            .to_string()
                    }
                };
            } else {
                replaced = CHANNEL_RE
                    .replace(&replaced, "#invalid-channel")
                    .to_string();
            }
        }

        for mat in ROLE_RE.find_iter(&msg.content) {
            let slice = &msg.content[mat.start() + 3..mat.end() - 1];
            let parsed: u64 = slice.parse().unwrap();

            let pinged_id = RoleId(parsed);

            if let Some(role) = roles.get(&pinged_id) {
                replaced = ROLE_RE
                    .replace(&replaced, format!("@{}", role.name))
                    .to_string();
            }
        }

        {
            use pulldown_cmark::Event::*;
            use pulldown_cmark::Tag::*;
            let parser = Parser::new(&replaced);

            for event in parser {
                match event {
                    Text(t) | Html(t) | Code(t) => computed.push_str(&format!("{} ", &t)),
                    End(_) => computed.push('\x0F'),
                    Start(tag) => match tag {
                        Emphasis => computed.push('\x1D'),
                        Strong => computed.push('\x02'),
                        Link(_, dest, _) => computed.push_str(&dest),
                        _ => {}
                    },
                    _ => {}
                }
            }
        }

        let chars = computed.chars().collect::<Vec<char>>();
        let chunks = chars.chunks(content_limit);

        if user_id != msg.author.id && !msg.author.bot && msg.channel_id == channel_id {
            if let Some(reply) = msg.referenced_message {
                let rnick = {
                    if let Some(member) = reply.member {
                        match member.nick {
                            Some(n) => n,
                            None => reply.author.name,
                        }
                    } else {
                        reply.author.name
                    }
                };
                let byte = rnick.chars().nth(0).unwrap() as u8;

                let colour_index = (byte as usize + rnick.len()) % 12;
                let formatted = format!("\x03{:02}", colour_index);
                let mut reply_nick = String::with_capacity(formatted.len() + rnick.len() + 2);

                reply_nick.push_str(&formatted);
                reply_nick.push_str(&rnick);
                reply_nick.push('\x0F');

                reply_nick = format!(
                    "{}\u{200B}{}",
                    &reply_nick[..formatted.len() + 1],
                    &reply_nick[formatted.len() + 1..]
                );
                let mut content = reply.content;
                content = content.replace('\n', " ");
                content = content.replace("\r\n", " "); // just in case
                content = format!(
                    "{} {}",
                    content, 
                    reply.attachments
                        .iter()
                        .map(|a| a.url.clone())
                        .collect::<Vec<String>>()
                        .join(" ")
                );
                let reply_nick_bytes = reply_nick.len() + 3;
                let reply_content_limit = 510 - reply_nick_bytes - 11;
                let to_send = if content.len() > reply_content_limit {
                    format!("{}...", &content[..reply_content_limit])
                } else {
                    content
                };
                send_irc_message(
                    &sender,
                    &channel,
                    &format!("(reply) <{}> {}", reply_nick, to_send),
                )
                .await
                .unwrap();
            }
            for chunk in chunks {
                let to_send = String::from_iter(chunk.iter());
                send_irc_message(&sender, &channel, &format!("<{}> {}", new_nick, to_send))
                    .await
                    .unwrap();
            }
            for attachment in attachments {
                send_irc_message(&sender, &channel, &format!("<{}> {}", new_nick, attachment))
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

    async fn guild_member_addition(&self, ctx: Context, _: GuildId, new_member: Member) {
        let members = {
            let data = ctx.data.read().await;
            let members = data.get::<MembersKey>().unwrap().to_owned();

            members
        };

        members.lock().await.push(new_member);
    }
}

struct HttpKey;
struct ChannelIdKey;
struct UserIdKey;
struct SenderKey;
struct MembersKey;
struct StringKey;

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
    type Value = Arc<Mutex<Vec<Member>>>;
}

impl TypeMapKey for StringKey {
    type Value = String;
}

#[cfg(unix)]
async fn terminate_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).unwrap();
    let mut sigint = signal(SignalKind::interrupt()).unwrap();

    select! {
        _ = sigterm.recv() => return,
        _ = sigint.recv() => return,
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
    let filename = env::args().nth(1).unwrap_or(String::from("config.toml"));
    let mut data = String::new();
    File::open(filename)?.read_to_string(&mut data)?;

    let conf: DircordConfig = toml::from_str(&data)?;

    let mut discord_client = DiscordClient::builder(&conf.token)
        .event_handler(Handler)
        .await?;

    let channel_id = ChannelId(conf.channel_id);

    let config = Config {
        nickname: conf.nickname,
        server: Some(conf.server),
        port: conf.port,
        channels: vec![conf.channel.clone()],
        use_tls: conf.tls,
        umodes: conf.mode,
        ..Config::default()
    };

    let irc_client = IrcClient::from_config(config).await?;

    let http = discord_client.cache_and_http.http.clone();

    let members = Arc::new(Mutex::new(
        channel_id
            .to_channel(discord_client.cache_and_http.clone())
            .await?
            .guild()
            .unwrap() // we can panic here because if it's not a guild channel then the bot shouldn't even work
            .guild_id
            .members(&http, None, None)
            .await?,
    ));

    {
        let mut data = discord_client.data.write().await;
        data.insert::<SenderKey>(irc_client.sender());
        data.insert::<MembersKey>(members.clone());
        data.insert::<ChannelIdKey>(channel_id);
        data.insert::<StringKey>(conf.channel);
    }

    let webhook = parse_webhook_url(http.clone(), conf.webhook)
        .await
        .expect("Invalid webhook URL");

    select! {
        r = irc_loop(irc_client, http.clone(), channel_id, webhook, members) => r?,
        r = discord_client.start() => r?,
        _ = terminate_signal() => {channel_id.say(&http, format!("dircord shutting down! (dircord {}-{})", env!("VERGEN_GIT_BRANCH"), &env!("VERGEN_GIT_SHA")[..7])).await?;},
    }

    Ok(())
}

async fn send_irc_message(sender: &Sender, channel: &str, content: &str) -> anyhow::Result<()> {
    sender.send_privmsg(channel, content)?;
    Ok(())
}

async fn irc_loop(
    mut client: IrcClient,
    http: Arc<Http>,
    channel_id: ChannelId,
    webhook: Option<Webhook>,
    members: Arc<Mutex<Vec<Member>>>,
) -> anyhow::Result<()> {
    let mut avatar_cache: HashMap<String, Option<String>> = HashMap::new();
    let mut id_cache: HashMap<String, Option<u64>> = HashMap::new();

    lazy_static! {
        static ref PING_NICK_1: Regex = Regex::new(r"^[\w+]+(:|,)").unwrap();
        static ref PING_RE_2: Regex = Regex::new(r"@[^0-9\s]+").unwrap();
        static ref CONTROL_CHAR_RE: Regex =
            Regex::new(r"\x1f|\x02|\x12|\x0f|\x16|\x03(?:\d{1,2}(?:,\d{1,2})?)?").unwrap();
        static ref WHITESPACE_RE: Regex = Regex::new(r"^\s").unwrap();
        static ref CHANNEL_RE: Regex = Regex::new(r"#[A-Za-z-*]+").unwrap();
    }

    client.identify()?;
    let mut stream = client.stream()?;

    let channels = channel_id
        .to_channel(&http)
        .await?
        .guild()
        .unwrap()
        .guild_id
        .channels(&http)
        .await?;

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
                        for member in &*members.lock().await {
                            let nick = match &member.nick {
                                Some(s) => s.to_owned(),
                                None => member.user.name.clone(),
                            };

                            if slice.starts_with(&nick) {
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
                        for member in &*members.lock().await {
                            let nick = match &member.nick {
                                Some(s) => s.to_owned(),
                                None => member.user.name.clone(),
                            };

                            if slice.starts_with(&nick) {
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
                    for member in &*members.lock().await {
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
                        let mut is_code = false;
                        if WHITESPACE_RE.is_match(&computed) {
                            computed = format!("`{}`", computed);
                            is_code = true;
                        }

                        if !is_code {
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

                            for mat in CHANNEL_RE.find_iter(message) {
                                let slice = &message[mat.start() + 1..mat.end()];

                                if let Some((id, _)) =
                                    channels.iter().find(|(_, c)| c.name == slice)
                                {
                                    computed = CHANNEL_RE
                                        .replace(&computed, format!("<#{}>", id.0))
                                        .to_string();
                                }
                            }

                            let mut has_opened_bold = false;
                            let mut has_opened_italic = false;

                            for c in computed.clone().chars() {
                                if c == '\x02' {
                                    computed = computed.replace('\x02', "**");
                                    has_opened_bold = true;
                                }

                                if c == '\x1D' {
                                    computed = computed.replace('\x1D', "*");
                                    has_opened_italic = true;
                                }

                                if c == '\x0F' {
                                    if has_opened_italic {
                                        computed = computed.replace('\x0F', "*");
                                        has_opened_italic = false;
                                    } else if has_opened_bold {
                                        computed = computed.replace('\x0F', "**");
                                        has_opened_bold = false;
                                    }
                                }
                            }

                            if has_opened_italic {
                                computed.push_str("*");
                            }

                            if has_opened_bold {
                                computed.push_str("**");
                            }

                            computed = CONTROL_CHAR_RE.replace_all(&computed, "").to_string();
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

                for mat in CHANNEL_RE.find_iter(message) {
                    let slice = &message[mat.start() + 1..mat.end()];

                    if let Some((id, _)) = channels.iter().find(|(_, c)| c.name == slice) {
                        computed = CHANNEL_RE
                            .replace(&computed, format!("<#{}>", id.0))
                            .to_string();
                    }
                }

                let mut has_opened_bold = false;
                let mut has_opened_italic = false;

                for c in computed.clone().chars() {
                    if c == '\x02' {
                        computed = computed.replace('\x02', "**");
                        has_opened_bold = true;
                    }

                    if c == '\x1D' {
                        computed = computed.replace('\x1D', "*");
                        has_opened_italic = true;
                    }

                    if c == '\x0F' {
                        if has_opened_italic {
                            computed = computed.replace('\x0F', "*");
                            has_opened_italic = false;
                        } else if has_opened_bold {
                            computed = computed.replace('\x0F', "**");
                            has_opened_bold = false;
                        }
                    }
                }

                if has_opened_italic {
                    computed.push_str("*");
                }

                if has_opened_bold {
                    computed.push_str("**");
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
        } else if let Command::NICK(ref new_nick) = orig_message.command {
            let old_nick = orig_message.source_nickname().unwrap();
            channel_id
                .say(
                    &http,
                    format!("*{}* is now known as *{}*", old_nick, new_nick),
                )
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
