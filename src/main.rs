use std::{env, sync::Arc};

use serenity::{prelude::*, async_trait, model::{channel::Message, id::ChannelId}, http::Http};

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        let nick = {
            if let Some(member) = msg.member {
                match member.nick {
                    Some(n) => n,
                    None => msg.author.name
                }
            } else {
                msg.author.name
            }
        };

        let (http, channel_id) = {
            let data = ctx.data.read().await;

            let http = data.get::<HttpKey>().unwrap().to_owned();
            let channel_id = data.get::<ChannelIdKey>().unwrap().to_owned();

            (http, channel_id)
        };

        send_message(&http, &channel_id, &format!("{}: {}", nick, msg.content)).await.unwrap();
    }
}

struct HttpKey;
struct ChannelIdKey;

impl TypeMapKey for HttpKey {
    type Value = Arc<Http>;
}

impl TypeMapKey for ChannelIdKey {
    type Value = ChannelId;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");

    let mut client = Client::builder(&token)
        .event_handler(Handler)
        .await?;

    let channel_id = ChannelId(831255708875751477);

    {
        let mut data = client.data.write().await;
        data.insert::<HttpKey>(client.cache_and_http.http.clone());
        data.insert::<ChannelIdKey>(channel_id);
    }

    client.start().await?;

    Ok(())
}

async fn send_message(http: &Http, channel_id: &ChannelId, content: &str) -> anyhow::Result<Message> {
    Ok(channel_id.say(http, content).await?)
}