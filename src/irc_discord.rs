use irc::{client::Client as IrcClient, proto::Command};

use std::{collections::HashMap, sync::Arc};

use tokio::sync::Mutex;

use serenity::{
    futures::StreamExt,
    http::Http,
    model::{
        prelude::{ChannelId, GuildChannel, Member, UserId},
        webhook::Webhook,
    },
    prelude::*,
};

use crate::{regex, OptionReplacer};

use fancy_regex::{Captures, Replacer};

macro_rules! unwrap_or_continue {
    ($opt:expr) => {
        match $opt {
            ::core::option::Option::Some(v) => v,
            ::core::option::Option::None => continue,
        }
    };
}

pub async fn irc_loop(
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