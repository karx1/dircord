use std::{env, sync::Arc};

use serenity::{
    async_trait,
    futures::StreamExt,
    http::Http,
    model::{
        channel::Message,
        id::{ChannelId, UserId},
        prelude::Ready,
    },
    prelude::*,
    Client as DiscordClient,
};

use irc::{
    client::{data::Config, Client as IrcClient, Sender},
    proto::Command,
};

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

        let (http, channel_id, user_id) = {
            let data = ctx.data.read().await;

            let http = data.get::<HttpKey>().unwrap().to_owned();
            let channel_id = data.get::<ChannelIdKey>().unwrap().to_owned();
            let user_id = data.get::<UserIdKey>().unwrap().to_owned();

            (http, channel_id, user_id)
        };

        // if user_id != msg.author.id && !msg.author.bot {
        //     send_message(&http, &channel_id, &format!("{}: {}", nick, msg.content))
        //         .await
        //         .unwrap();
        // }
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
    let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");

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

    let mut irc_client = IrcClient::from_config(config).await?;

    let http = discord_client.cache_and_http.http.clone();

    {
        let mut data = discord_client.data.write().await;
        data.insert::<HttpKey>(http.clone());
        data.insert::<ChannelIdKey>(channel_id);
        data.insert::<SenderKey>(irc_client.sender());
    }

    tokio::spawn(async move {
        irc_loop(irc_client, http, channel_id).await.unwrap();
    });
    discord_client.start().await?;

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
        if let Command::PRIVMSG(ref channel, ref message) = orig_message.command {
            let nickname = orig_message.source_nickname().unwrap();
            channel_id
                .say(&http, format!("{}: {}", nickname, message))
                .await?;
        }
    }
    Ok(())
}
