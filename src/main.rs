use std::{env, sync::Arc, fs::File, io::Read};

use serenity::{
    async_trait,
    futures::StreamExt,
    http::Http,
    model::{
        channel::Message,
        id::{ChannelId, UserId},
        prelude::Ready, webhook::Webhook,
    },
    prelude::*,
    Client as DiscordClient,
};

use irc::{
    client::{data::Config, Client as IrcClient, Sender},
    proto::Command,
};

use toml::Value;

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

        let (user_id, sender) = {
            let data = ctx.data.read().await;

            let user_id = data.get::<UserIdKey>().unwrap().to_owned();
            let sender = data.get::<SenderKey>().unwrap().to_owned();

            (user_id, sender)
        };
        
        if user_id != msg.author.id && !msg.author.bot {
            send_irc_message(&sender, &format!("<{}> {}", nick, msg.content))
                .await
                .unwrap();
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filename = env::args().nth(1).expect("No filename was provided!");
    let mut data = String::new();
    File::open(filename)?.read_to_string(&mut data)?;

    let value = data.parse::<Value>()?;

    let token = value["token"].as_str().expect("No token provided!").to_string();
    let webhook = value["webhook"].as_str().map(|s| s.to_string());

    let mut discord_client = DiscordClient::builder(&token)
        .event_handler(Handler)
        .await?;

    let channel_id = ChannelId(831255708875751477);

    let config = Config {
        nickname: Some("dircord".to_string()),
        server: Some("192.168.1.28".to_owned()),
        port: Some(6667),
        channels: vec!["#no-normies".to_string()],
        use_tls: Some(false),
        umodes: Some("+B".to_string()),
        ..Config::default()
    };

    let irc_client = IrcClient::from_config(config).await?;

    let http = discord_client.cache_and_http.http.clone();

    {
        let mut data = discord_client.data.write().await;
        data.insert::<SenderKey>(irc_client.sender());
    }

    let webhook = parse_webhook_url(http.clone(), webhook).await;

    tokio::spawn(async move {
        irc_loop(irc_client, http, channel_id).await.unwrap();
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
) -> anyhow::Result<()> {
    client.identify()?;
    let mut stream = client.stream()?;
    while let Some(orig_message) = stream.next().await.transpose()? {
        print!("{}", orig_message);
        if let Command::PRIVMSG(_, ref message) = orig_message.command {
            let nickname = orig_message.source_nickname().unwrap();
            channel_id
                .say(&http, format!("{}: {}", nickname, message))
                .await?;
        }
    }
    Ok(())
}

async fn parse_webhook_url(http: Arc<Http>, url: Option<String>) -> Option<Webhook> {
    if let Some(url) = url {
        let url = url.trim_start_matches("https://discord.com/api/webhooks/");
        let split = url.split("/").collect::<Vec<&str>>();
        let id = split[0].parse::<u64>().expect("Invalid webhook URL");
        let token = split[1].to_string();
        let webhook = http.get_webhook_with_token(id, &token).await.expect("Invalid webhook URL");
        Some(webhook)
    } else {
        None
    }
}