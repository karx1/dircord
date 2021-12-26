use std::env;

use serenity::{prelude::*, async_trait, model::{channel::Message, id::ChannelId}, http::CacheHttp};

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

        msg.channel_id.say(&ctx, format!("{}: {}", nick, msg.content)).await.unwrap();
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");

    let mut client = Client::builder(&token)
        .event_handler(Handler)
        .await?;

    let channel_id = ChannelId(831255708875751477);

    client.start().await?;

    Ok(())
}

async fn send_message(client: &Client, channel_id: &ChannelId, content: &str) -> anyhow::Result<Message> {
    let http = client.cache_and_http.http();

    Ok(channel_id.say(http, content).await?)
}