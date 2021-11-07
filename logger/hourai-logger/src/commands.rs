use anyhow::Result;
use futures::{channel::mpsc, future, prelude::*};
use hourai::interactions::{Command, CommandContext, CommandError, Response};
use hourai::{
    http::request::prelude::AuditLogReason,
    models::{
        channel::message::{allowed_mentions::AllowedMentions, Message},
        guild::{Guild, Permissions},
        id::{ChannelId, MessageId, UserId},
        user::User,
    },
    proto::guild_configs::LoggingConfig,
};
use hourai_redis::{CachedGuild, GuildConfig, RedisPool};
use hourai_sql::SqlPool;
use hourai_storage::Storage;
use rand::Rng;
use regex::Regex;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_PRUNED_MESSAGES: usize = 100;
const MAX_PRUNED_MESSAGES_PER_BATCH: usize = 100;

pub async fn handle_command(ctx: CommandContext, mut storage: Storage) -> Result<()> {
    let result = match ctx.command() {
        Command::Command("pingmod") => pingmod(&ctx, &mut storage).await,

        // Admin Commands
        Command::Command("ban") => ban(&ctx, &mut storage).await,
        Command::Command("kick") => kick(&ctx, &mut storage).await,
        Command::Command("mute") => mute(&ctx).await,
        Command::Command("move") => move_cmd(&ctx, &mut storage).await,
        Command::Command("deafen") => deafen(&ctx).await,
        Command::Command("prune") => prune(&ctx).await,
        _ => return Err(anyhow::Error::new(CommandError::UnknownCommand)),
    };

    match result {
        Ok(response) => ctx.reply(response).await,
        Err(err) => {
            let response = Response::ephemeral();
            if let Some(command_err) = err.downcast_ref::<CommandError>() {
                ctx.reply(response.content(format!(":x: Error: {}", command_err)))
                    .await?;
                Ok(())
            } else {
                // TODO(james7132): Add some form of tracing for this.
                ctx.reply(response.content(":x: Fatal Error: Internal Error has occured."))
                    .await?;
                Err(err)
            }
        }
    }
}

async fn pingmod(ctx: &CommandContext, storage: &Storage) -> Result<Response> {
    let guild_id = ctx.guild_id()?;
    let mut redis = storage.redis().clone();
    let online_mods =
        hourai_storage::find_online_moderators(guild_id, storage.sql(), &mut redis).await?;
    let guild = CachedGuild::fetch_resource::<Guild>(guild_id, guild_id, &mut redis)
        .await?
        .ok_or(CommandError::NotInGuild)?;
    let config = GuildConfig::fetch_or_default::<LoggingConfig>(guild_id, &mut redis).await?;

    let mention: String;
    let ping: String;
    if online_mods.is_empty() {
        mention = format!("<@{}>", guild.get_owner_id());
        ping = format!("<@{}>, No mods online!", guild.get_owner_id());
    } else {
        let idx = rand::thread_rng().gen_range(0..online_mods.len());
        mention = format!("<@{}>", online_mods[idx].user_id());
        ping = mention.clone();
    };

    let content = ctx
        .get_string("reason")
        .map(|reason| format!("{}: {}", ping, reason))
        .unwrap_or(ping);

    if config.has_modlog_channel_id() {
        ctx.http
            .create_message(ChannelId::new(config.get_modlog_channel_id()).unwrap())
            .content(&format!(
                "<@{}> used `/pingmod` to ping {} in <#{}>",
                ctx.user().id,
                mention,
                ctx.channel_id()
            ))?
            .allowed_mentions(AllowedMentions::builder().build())
            .exec()
            .await?;

        ctx.http
            .create_message(ctx.channel_id())
            .content(&content)?
            .exec()
            .await?;

        Ok(Response::ephemeral().content(format!("Pinged {} to this channel.", mention)))
    } else {
        Ok(Response::direct().content(&content))
    }
}

fn build_reason(action: &str, authorizer: &User, reason: Option<&String>) -> String {
    if let Some(reason) = reason {
        format!(
            "{} by {}#{} for: {}",
            action, authorizer.name, authorizer.discriminator, reason
        )
    } else {
        format!(
            "{} by {}#{}",
            action, authorizer.name, authorizer.discriminator
        )
    }
}

async fn ban(ctx: &CommandContext, storage: &mut Storage) -> Result<Response> {
    let guild_id = ctx.guild_id()?;
    let soft = ctx.get_flag("soft").unwrap_or(false);
    let action = if soft { "Softbanned" } else { "Banned" };
    let authorizer = ctx.command.member.as_ref().expect("Command without user.");
    let authorizer_roles =
        CachedGuild::role_set(guild_id, &authorizer.roles, &mut storage.redis().clone()).await?;
    let reason = build_reason(
        action,
        authorizer.user.as_ref().unwrap(),
        ctx.get_string("reason").ok(),
    );

    let duration = ctx.get_string("duration");
    if duration.is_ok() {
        anyhow::bail!(CommandError::UserError(
            "Temp bans via this command are currently not supported.",
        ));
    }

    // TODO(james7132): Properly display the errors.
    let users: Vec<_> = ctx.all_users("user").collect();
    let mut errors = Vec::new();
    if soft {
        if !ctx.has_user_permission(Permissions::KICK_MEMBERS) {
            anyhow::bail!(CommandError::MissingPermission("Kick Members"));
        }

        for user_id in users.iter() {
            if let Some(member) = ctx.resolve_member(*user_id) {
                let roles =
                    CachedGuild::role_set(guild_id, &member.roles, &mut storage.redis().clone())
                        .await?;
                if roles >= authorizer_roles {
                    errors.push(format!(
                        "{}: Has higher roles, not authorized to softban.",
                        user_id
                    ));
                    continue;
                }
            }

            let request = ctx
                .http
                .create_ban(guild_id, *user_id)
                .delete_message_days(7)
                .unwrap()
                .reason(&reason)
                .unwrap();
            if let Err(err) = request.exec().await {
                errors.push(format!("{}: {}", user_id, err));
                continue;
            }

            let request = ctx
                .http
                .delete_ban(guild_id, *user_id)
                .reason(&reason)
                .unwrap();
            if let Err(err) = request.exec().await {
                tracing::error!("Error while running /ban on {}: {}", user_id, err);
                errors.push(format!("{}: {}", user_id, err));
            }
        }
    } else {
        if !ctx.has_user_permission(Permissions::BAN_MEMBERS) {
            anyhow::bail!(CommandError::MissingPermission("Ban Members"));
        }

        for user_id in users.iter() {
            if let Some(member) = ctx.resolve_member(*user_id) {
                let roles =
                    CachedGuild::role_set(guild_id, &member.roles, &mut storage.redis().clone())
                        .await?;
                if roles >= authorizer_roles {
                    errors.push(format!(
                        "{}: Has higher roles, not authorized to ban.",
                        user_id
                    ));
                    continue;
                }
            }

            let request = ctx
                .http
                .create_ban(guild_id, *user_id)
                .reason(&reason)
                .unwrap();
            if let Err(err) = request.exec().await {
                tracing::error!("Error while running /ban on {}: {}", user_id, err);
                errors.push(format!("{}: {}", user_id, err));
            }
        }
    }
    Ok(Response::direct().content(format!("{} {} users.", action, users.len() - errors.len())))
}

async fn kick(ctx: &CommandContext, storage: &mut Storage) -> Result<Response> {
    let guild_id = ctx.guild_id()?;
    if !ctx.has_user_permission(Permissions::KICK_MEMBERS) {
        anyhow::bail!(CommandError::MissingPermission("Kick Members"));
    }

    let authorizer = ctx.command.member.as_ref().expect("Command without user.");
    let authorizer_roles =
        CachedGuild::role_set(guild_id, &authorizer.roles, &mut storage.redis().clone()).await?;
    let reason = build_reason(
        "Kicked",
        authorizer.user.as_ref().unwrap(),
        ctx.get_string("reason").ok(),
    );

    let members: Vec<_> = ctx.all_users("user").collect();
    let mut errors = Vec::new();
    for member_id in members.iter() {
        if let Some(member) = ctx.resolve_member(*member_id) {
            let roles =
                CachedGuild::role_set(guild_id, &member.roles, &mut storage.redis().clone())
                    .await?;
            if roles >= authorizer_roles {
                errors.push(format!(
                    "{}: Has higher or equal roles, not authorized to kick.",
                    member_id
                ));
                continue;
            }
        }

        let request = ctx
            .http
            .remove_guild_member(guild_id, *member_id)
            .reason(&reason)
            .unwrap();
        if let Err(err) = request.exec().await {
            tracing::error!("Error while running /kick on {}: {}", member_id, err);
            errors.push(format!("{}: {}", member_id, err));
        }
    }

    Ok(Response::direct().content(format!("Kicked {} users.", members.len() - errors.len())))
}

async fn deafen(ctx: &CommandContext) -> Result<Response> {
    let guild_id = ctx.guild_id()?;
    if !ctx.has_user_permission(Permissions::DEAFEN_MEMBERS) {
        anyhow::bail!(CommandError::MissingPermission("Deafen Members"));
    }

    let authorizer = ctx.member().expect("Command without user.");
    let reason = build_reason(
        "Deafened",
        authorizer.user.as_ref().unwrap(),
        ctx.get_string("reason").ok(),
    );

    let members: Vec<_> = ctx.all_users("user").collect();
    let mut errors = Vec::new();
    for member_id in members.iter() {
        let request = ctx
            .http
            .update_guild_member(guild_id, *member_id)
            .deaf(true)
            .reason(&reason)
            .unwrap();
        if let Err(err) = request.exec().await {
            tracing::error!("Error while running /deafen on {}: {}", member_id, err);
            errors.push(format!("{}: {}", member_id, err));
        }
    }

    Ok(Response::direct().content(format!("Deafened {} users.", members.len() - errors.len())))
}

async fn mute(ctx: &CommandContext) -> Result<Response> {
    let guild_id = ctx.guild_id()?;
    if !ctx.has_user_permission(Permissions::MUTE_MEMBERS) {
        anyhow::bail!(CommandError::MissingPermission("Mute Members"));
    }

    let authorizer = ctx.member().expect("Command without user.");
    let reason = build_reason(
        "Muted",
        authorizer.user.as_ref().unwrap(),
        ctx.get_string("reason").ok(),
    );

    let members: Vec<_> = ctx.all_users("user").collect();
    let mut errors = Vec::new();
    for member_id in members.iter() {
        let request = ctx
            .http
            .update_guild_member(guild_id, *member_id)
            .mute(true)
            .reason(&reason)
            .unwrap();
        if let Err(err) = request.exec().await {
            tracing::error!("Error while running /mute on {}: {}", member_id, err);
            errors.push(format!("{}: {}", member_id, err));
        }
    }

    Ok(Response::direct().content(format!("Muted {} users.", members.len() - errors.len())))
}

async fn move_cmd(ctx: &CommandContext, storage: &mut Storage) -> Result<Response> {
    let guild_id = ctx.guild_id()?;
    if !ctx.has_user_permission(Permissions::MOVE_MEMBERS) {
        anyhow::bail!(CommandError::MissingPermission("Move Members"));
    }

    let authorizer = ctx.member().expect("Command without user.");
    let reason = build_reason(
        "Moved",
        authorizer.user.as_ref().unwrap(),
        ctx.get_string("reason").ok(),
    );

    let states: HashMap<u64, u64> = hourai_redis::CachedVoiceState::get_channels(guild_id)
        .query_async(&mut storage.redis().clone())
        .await?;

    let src = ctx.get_channel("src")?;
    let dst = ctx.get_channel("dst")?;

    let mut success = 0;
    let mut errors = Vec::new();
    for (user_id, channel_id) in states {
        if ChannelId::new(channel_id) != Some(src) {
            continue;
        }
        if let Some(user_id) = UserId::new(user_id) {
            let request = ctx
                .http
                .update_guild_member(guild_id, user_id)
                .channel_id(Some(dst))
                .reason(&reason)
                .unwrap();
            if let Err(err) = request.exec().await {
                tracing::error!("Error while running /mute on {}: {}", user_id, err);
                errors.push(format!("{}: {}", user_id, err));
            } else {
                success += 1;
            }
        }
    }

    Ok(Response::direct().content(format!("Moved {} users.", success)))
}

async fn fetch_messages(
    channel_id: ChannelId,
    http: Arc<hourai::http::Client>,
    tx: mpsc::UnboundedSender<Message>,
) -> Result<()> {
    const TWO_WEEKS_SECS: u64 = 14 * 24 * 60 * 60;
    let limit = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - TWO_WEEKS_SECS;
    let mut oldest = MessageId::new(u64::MAX).unwrap();
    loop {
        let messages = http
            .channel_messages(channel_id)
            .before(oldest)
            .exec()
            .await?
            .model()
            .await?;
        for message in messages {
            oldest = std::cmp::min(oldest, message.id);
            if message.timestamp.as_secs() < limit {
                return Ok(());
            } else {
                tx.unbounded_send(message)?;
            }
        }
    }
}

async fn prune(ctx: &CommandContext) -> Result<Response> {
    ctx.guild_id()?;
    let count = ctx.get_int("count").unwrap_or(100) as usize;
    if count > MAX_PRUNED_MESSAGES {
        anyhow::bail!(CommandError::InvalidArgument(
            "Prune only supports up to 2000 messages."
        ));
    }

    let mut filters: Vec<Box<dyn Fn(&Message) -> bool + Send + 'static>> = Vec::new();
    let mine = ctx.get_flag("mine").unwrap_or(false);
    if mine {
        let user_id = ctx.user().id;
        filters.push(Box::new(move |msg| msg.author.id == user_id));
    }
    if ctx.get_flag("bot").unwrap_or(false) {
        filters.push(Box::new(|msg| msg.author.bot));
    }
    if ctx.get_flag("embed").unwrap_or(false) {
        filters.push(Box::new(|msg| {
            !msg.embeds.is_empty() || !msg.attachments.is_empty()
        }));
    }
    if ctx.get_flag("mention").unwrap_or(false) {
        filters.push(Box::new(|msg| {
            msg.mention_everyone || !msg.mention_roles.is_empty() || !msg.mentions.is_empty()
        }));
    }
    if let Ok(user) = ctx.get_user("user") {
        let user_id = user;
        filters.push(Box::new(move |msg| msg.author.id == user_id));
    }
    if let Ok(rgx) = ctx.get_string("match") {
        let regex = Regex::new(&rgx).map_err(|_| {
            CommandError::InvalidArgument("`match` must be a valid regex or pattern.")
        })?;
        filters.push(Box::new(move |msg| regex.is_match(&msg.content)));
    }

    if !mine && !ctx.has_user_permission(Permissions::MANAGE_MESSAGES) {
        anyhow::bail!(CommandError::MissingPermission("Manage Messages"));
    }

    let authorizer = ctx.member().expect("Command without user.");
    let reason = build_reason(
        "Pruned",
        authorizer.user.as_ref().unwrap(),
        ctx.get_string("reason").ok(),
    );

    let (tx, rx) = mpsc::unbounded();
    tokio::spawn(fetch_messages(ctx.channel_id(), ctx.http.clone(), tx));

    let batches: Vec<Vec<MessageId>> = rx
        .take(count)
        .filter(move |msg| future::ready(filters.iter().all(|f| f(msg))))
        .map(|msg| msg.id)
        .chunks(MAX_PRUNED_MESSAGES_PER_BATCH)
        .map(|batch| Vec::from(batch))
        .collect()
        .await;

    let mut total = 0;
    for batch in batches {
        ctx.http
            .delete_messages(ctx.channel_id(), &batch)
            .reason(&reason)?
            .exec()
            .await?;
        total += batch.len();
    }
    Ok(Response::direct().content(format!("Pruned {} messages.", total)))
}
