mod compression;
mod guild_config;
mod keys;
pub mod modlog;
mod protobuf;

pub use redis::*;

use self::compression::Compressed;
pub use self::guild_config::CachedGuildConfig;
use self::keys::{CacheKey, GuildKey, Id};
use self::protobuf::Protobuf;
use anyhow::Result;
use hourai::{
    gateway::shard::ResumeSession,
    models::{
        channel::GuildChannel,
        guild::{Guild, PartialGuild, Permissions, Role},
        id::*,
        voice::VoiceState,
        MessageLike, Snowflake, UserLike,
    },
    proto::{cache::*, music_bot::MusicStateProto},
};
use redis::{aio::ConnectionLike, FromRedisValue, ToRedisArgs};
use std::{
    cmp::{Ord, Ordering},
    collections::{HashMap, HashSet},
    hash::Hash,
    ops::Deref,
};
use tracing::debug;

pub type RedisPool = redis::aio::ConnectionManager;

pub async fn init(config: &hourai::config::HouraiConfig) -> RedisPool {
    debug!("Creating Redis client");
    let client = redis::Client::open(config.redis.as_ref()).expect("Failed to create Redis client");
    RedisPool::new(client)
        .await
        .expect("Failed to initialize multiplexed Redis connection")
}

pub struct OnlineStatus {
    pipeline: redis::Pipeline,
}

impl Default for OnlineStatus {
    fn default() -> Self {
        Self {
            pipeline: redis::pipe().atomic().clone(),
        }
    }
}

impl OnlineStatus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_online(
        &mut self,
        guild_id: GuildId,
        online: impl IntoIterator<Item = UserId>,
    ) -> &mut Self {
        let key = CacheKey::OnlineStatus(guild_id.get());
        let ids: Vec<Id<u64>> = online.into_iter().map(|id| Id(id.get())).collect();
        self.pipeline
            .del(key.clone())
            .ignore()
            .sadd(key.clone(), ids)
            .ignore()
            .expire(key.clone(), 3600);
        self
    }

    pub async fn find_online(
        guild_id: GuildId,
        users: impl IntoIterator<Item = UserId>,
        redis: &mut RedisPool,
    ) -> Result<HashSet<UserId>> {
        let key = CacheKey::OnlineStatus(guild_id.get());
        let user_ids: Vec<UserId> = users.into_iter().collect();
        let mut pipe = redis::pipe();
        user_ids.iter().map(|id| Id(id.get())).for_each(|id| {
            pipe.sismember(key.clone(), id);
        });
        let results: Vec<bool> = pipe.query_async(redis).await?;
        Ok(user_ids
            .into_iter()
            .zip(results)
            .filter(|(_, online)| *online)
            .map(|(id, _)| id)
            .collect())
    }

    pub fn build(self) -> redis::Pipeline {
        self.pipeline
    }
}

pub struct GuildConfig;

impl GuildConfig {
    pub async fn fetch<T: ::protobuf::Message + CachedGuildConfig, C: ConnectionLike>(
        id: GuildId,
        conn: &mut C,
    ) -> std::result::Result<Option<T>, redis::RedisError> {
        let key = CacheKey::GuildConfigs(id.get());
        let response: Option<Compressed<Protobuf<T>>> = redis::Cmd::hget(key, vec![T::SUBKEY])
            .query_async(conn)
            .await?;
        Ok(response.map(|c| c.0 .0))
    }

    pub async fn fetch_or_default<T: ::protobuf::Message + CachedGuildConfig, C: ConnectionLike>(
        id: GuildId,
        conn: &mut C,
    ) -> std::result::Result<T, redis::RedisError> {
        Ok(Self::fetch::<T, C>(id, conn).await?.unwrap_or_else(T::new))
    }

    pub fn set<T: ::protobuf::Message + CachedGuildConfig>(id: GuildId, value: T) -> redis::Cmd {
        let key = CacheKey::GuildConfigs(id.get());
        redis::Cmd::hset(key, vec![T::SUBKEY], Compressed(Protobuf(value)))
    }
}

pub struct CachedMessage {
    proto: Protobuf<CachedMessageProto>,
}

impl CachedMessage {
    pub fn new(message: impl MessageLike) -> Self {
        let mut msg = CachedMessageProto::new();
        msg.set_id(message.id().get());
        msg.set_channel_id(message.channel_id().get());
        msg.set_content(message.content().to_owned());
        if let Some(guild_id) = message.guild_id() {
            msg.set_guild_id(guild_id.get())
        }

        let user = msg.mut_author();
        let author = message.author();
        user.set_id(author.id().get());
        user.set_username(author.name().to_owned());
        user.set_discriminator(author.discriminator() as u32);
        user.set_bot(author.bot());
        if let Some(avatar) = author.avatar_hash() {
            user.set_avatar(avatar.to_owned());
        }

        Self {
            proto: Protobuf(msg),
        }
    }

    pub async fn fetch<C: ConnectionLike>(
        channel_id: ChannelId,
        message_id: MessageId,
        conn: &mut C,
    ) -> Result<Option<CachedMessageProto>> {
        let key = CacheKey::Messages(channel_id.get(), message_id.get());
        let proto: Option<Protobuf<CachedMessageProto>> =
            redis::Cmd::get(key).query_async(conn).await?;
        Ok(proto.map(|msg| {
            let mut cached_message = msg.0;
            cached_message.set_id(message_id.get());
            cached_message.set_channel_id(channel_id.get());
            cached_message
        }))
    }

    pub fn flush(mut self) -> redis::Cmd {
        let channel_id = self.proto.0.get_channel_id();
        let id = self.proto.0.get_id();
        let key = CacheKey::Messages(channel_id, id);
        // Remove IDs to save space, as it's in the key.
        self.proto.0.clear_id();
        self.proto.0.clear_channel_id();
        // Keep 1 day's worth of messages cached.
        redis::Cmd::set_ex(key, self.proto, 86400)
    }

    pub fn delete(channel_id: ChannelId, id: MessageId) -> redis::Cmd {
        Self::bulk_delete(channel_id, vec![id])
    }

    pub fn bulk_delete(
        channel_id: ChannelId,
        ids: impl IntoIterator<Item = MessageId>,
    ) -> redis::Cmd {
        let keys: Vec<CacheKey> = ids
            .into_iter()
            .map(|id| CacheKey::Messages(channel_id.get(), id.get()))
            .collect();
        redis::Cmd::del(keys)
    }
}

pub struct CachedVoiceState;

impl CachedVoiceState {
    pub fn update_guild(guild: &Guild) -> redis::Pipeline {
        let mut pipe = redis::pipe();
        pipe.atomic()
            .del(CacheKey::VoiceState(guild.id.get()))
            .ignore();
        for state in guild.voice_states.iter() {
            pipe.add_command(Self::save(state)).ignore();
        }
        pipe
    }

    pub fn get_channel(guild_id: GuildId, user_id: UserId) -> redis::Cmd {
        redis::Cmd::hget(CacheKey::VoiceState(guild_id.get()), user_id.get())
    }

    pub fn get_channels(guild_id: GuildId) -> redis::Cmd {
        redis::Cmd::hgetall(CacheKey::VoiceState(guild_id.get()))
    }

    pub fn save(state: &VoiceState) -> redis::Cmd {
        let guild_id = state
            .guild_id
            .expect("Only voice states in guilds should be cached");
        let key = CacheKey::VoiceState(guild_id.get());
        if let Some(channel_id) = state.channel_id {
            redis::Cmd::hset(key, state.user_id.get(), channel_id.get())
        } else {
            redis::Cmd::hdel(key, state.user_id.get())
        }
    }

    pub fn clear_guild(guild_id: GuildId) -> redis::Cmd {
        redis::Cmd::del(CacheKey::VoiceState(guild_id.get()))
    }
}

pub struct CachedGuild;

impl CachedGuild {
    pub fn save(guild: &hourai::models::guild::Guild) -> redis::Pipeline {
        let key = CacheKey::Guild(guild.id.get());
        let mut pipe = redis::pipe();
        pipe.atomic().del(key).ignore();
        pipe.add_command(Self::save_resource(guild.id, guild.id, guild))
            .ignore();
        for channel in guild.channels.iter() {
            pipe.add_command(Self::save_resource(guild.id, channel.id(), channel))
                .ignore();
        }
        for role in guild.roles.iter() {
            pipe.add_command(Self::save_resource(guild.id, role.id, role))
                .ignore();
        }
        pipe
    }

    /// Deletes all of the cached information about a guild from the cache.
    pub fn delete(guild_id: GuildId) -> redis::Cmd {
        redis::Cmd::del(CacheKey::Guild(guild_id.get()))
    }

    /// Gets a cached resource from the cache.
    pub async fn fetch_resource<T: GuildResource>(
        guild_id: GuildId,
        resource_id: T::Id,
        conn: &mut RedisPool,
    ) -> Result<Option<T::Proto>>
    where
        GuildKey: From<T::Id> + ToRedisArgs,
    {
        let guild_key = CacheKey::Guild(guild_id.get());
        let proto: Option<Protobuf<T::Proto>> = redis::Cmd::hget(guild_key, resource_id.into())
            .query_async(conn)
            .await?;
        Ok(proto.map(|proto| proto.0))
    }

    /// Fetches multiple resources from the cache.
    pub async fn fetch_all_resources<T: GuildResource>(
        guild_id: GuildId,
        conn: &mut RedisPool,
    ) -> Result<HashMap<T::Id, T::Proto>>
    where
        GuildKey: From<T::Id> + ToRedisArgs,
    {
        // TODO(james7132): Using HGETALL here is super inefficient with guilds with high
        // role/channel counts, see if this is avoidable.
        let guild_key = CacheKey::Guild(guild_id.get());
        let response: HashMap<GuildKey, redis::Value> =
            redis::Cmd::hgetall(guild_key).query_async(conn).await?;
        let mut protos = HashMap::new();
        for (key, value) in response.into_iter() {
            if key.prefix() != T::PREFIX {
                continue;
            }
            let proto = Protobuf::<T::Proto>::from_redis_value(&value)?;
            protos.insert(T::from_key(key), proto.0);
        }

        Ok(protos)
    }

    /// Fetches multiple resources from the cache.
    pub async fn fetch_resources<T: GuildResource>(
        guild_id: GuildId,
        resource_ids: &[T::Id],
        conn: &mut RedisPool,
    ) -> Result<Vec<T::Proto>>
    where
        GuildKey: From<T::Id> + ToRedisArgs,
    {
        Ok(match resource_ids.len() {
            0 => vec![],
            1 => Self::fetch_resource::<T>(guild_id, resource_ids[0], conn)
                .await?
                .into_iter()
                .collect(),
            _ => {
                let guild_key = CacheKey::Guild(guild_id.get());
                let resource_keys: Vec<GuildKey> =
                    resource_ids.iter().map(|id| id.clone().into()).collect();
                let protos: Vec<Option<Protobuf<T::Proto>>> =
                    redis::Cmd::hget(guild_key, resource_keys)
                        .query_async(conn)
                        .await?;
                protos
                    .into_iter()
                    .filter_map(|p| p.map(|proto| proto.0))
                    .collect()
            }
        })
    }

    /// Saves a resoruce into the cache.
    pub fn save_resource<T: GuildResource>(
        guild_id: GuildId,
        resource_id: T::Id,
        data: &T,
    ) -> redis::Cmd
    where
        GuildKey: From<T::Id> + ToRedisArgs,
    {
        let proto = Protobuf(data.to_proto());
        redis::Cmd::hset(CacheKey::Guild(guild_id.get()), resource_id.into(), proto)
    }

    /// Deletes a resource from the cache.
    pub fn delete_resource<T: GuildResource>(guild_id: GuildId, resource_id: T::Id) -> redis::Cmd
    where
        GuildKey: From<T::Id> + ToRedisArgs,
    {
        redis::Cmd::hdel(CacheKey::Guild(guild_id.get()), resource_id.into())
    }

    /// Fetches a `RoleSet` from the provided guild and role IDs.
    pub async fn role_set(
        guild_id: GuildId,
        role_ids: &[RoleId],
        conn: &mut RedisPool,
    ) -> Result<RoleSet> {
        Ok(RoleSet(
            Self::fetch_resources::<Role>(guild_id, role_ids, conn).await?,
        ))
    }

    /// Gets the guild-level permissions for a given member.
    /// If the guild or any of the roles are not present, this will return
    /// Permissions::empty.
    pub async fn guild_permissions(
        guild_id: GuildId,
        user_id: UserId,
        role_ids: impl Iterator<Item = RoleId>,
        conn: &mut RedisPool,
    ) -> Result<Permissions> {
        // The owner has all permissions.
        if let Some(guild) = Self::fetch_resource::<Guild>(guild_id, guild_id, conn).await? {
            if guild.get_owner_id() == user_id.get() {
                return Ok(Permissions::all());
            }
        } else {
            return Ok(Permissions::empty());
        }

        // The everyone role ID is the same as the guild ID.
        let mut role_ids: Vec<RoleId> = role_ids.collect();
        role_ids.push(RoleId(guild_id.0));
        Ok(Self::role_set(guild_id, &role_ids, conn)
            .await?
            .guild_permissions())
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RoleSet(Vec<CachedRoleProto>);

impl RoleSet {
    /// Gets the highest role in the `RoleSet`, if available. Returns None if the set is empty.
    pub fn highest(&self) -> Option<&CachedRoleProto> {
        self.0.iter().max()
    }

    /// Computes the available permissions for all of the roles.
    pub fn guild_permissions(&self) -> Permissions {
        let perms = self
            .0
            .iter()
            .map(|role| Permissions::from_bits_truncate(role.get_permissions()))
            .fold(Permissions::empty(), |acc, perm| acc | perm);

        // Administrators by default have every permission enabled.
        if perms.contains(Permissions::ADMINISTRATOR) {
            Permissions::all()
        } else {
            perms
        }
    }
}

impl Ord for RoleSet {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.highest(), other.highest()) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => Ordering::Equal,
        }
    }
}

impl PartialOrd for RoleSet {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Deref for RoleSet {
    type Target = Vec<CachedRoleProto>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub trait ToProto {
    type Proto: ::protobuf::Message;
    fn to_proto(&self) -> Self::Proto;
}

pub trait GuildResource: ToProto {
    type Id: Into<GuildKey> + Copy + Eq + Hash;
    type Subkey;
    const PREFIX: u8;

    fn from_key(id: GuildKey) -> Self::Id;
}

impl GuildResource for Guild {
    type Id = GuildId;
    type Subkey = ();
    const PREFIX: u8 = 1_u8;

    fn from_key(_: GuildKey) -> Self::Id {
        panic!("Converting GuildKey to GuildId is not supported");
    }
}

impl ToProto for Guild {
    type Proto = CachedGuildProto;
    fn to_proto(&self) -> Self::Proto {
        let mut proto = Self::Proto::new();
        proto.set_id(self.id.get());
        proto.set_name(self.name.clone());
        proto.features = ::protobuf::RepeatedField::from_vec(self.features.clone());
        proto.set_owner_id(self.owner_id.get());
        if let Some(ref code) = self.vanity_url_code {
            proto.set_vanity_url_code(code.clone());
        }
        proto
    }
}

impl GuildResource for PartialGuild {
    type Id = GuildId;
    type Subkey = ();
    const PREFIX: u8 = 1_u8;

    fn from_key(_: GuildKey) -> Self::Id {
        panic!("Converting GuildKey to GuildId is not supported");
    }
}

impl ToProto for PartialGuild {
    type Proto = CachedGuildProto;
    fn to_proto(&self) -> Self::Proto {
        let mut proto = Self::Proto::new();
        proto.set_id(self.id.get());
        proto.set_name(self.name.clone());
        proto.features = ::protobuf::RepeatedField::from_vec(self.features.clone());
        proto.set_owner_id(self.owner_id.get());
        if let Some(ref code) = self.vanity_url_code {
            proto.set_vanity_url_code(code.clone());
        }
        proto
    }
}

impl GuildResource for GuildChannel {
    type Id = ChannelId;
    type Subkey = u64;
    const PREFIX: u8 = 3_u8;

    fn from_key(key: GuildKey) -> Self::Id {
        if let GuildKey::Channel(id) = key {
            id
        } else {
            panic!("Invalid GuildKey for channel: {:?}", key);
        }
    }
}

impl ToProto for GuildChannel {
    type Proto = CachedGuildChannelProto;
    fn to_proto(&self) -> Self::Proto {
        let mut proto = Self::Proto::new();
        proto.set_channel_id(self.id().get());
        proto.set_name(self.name().to_owned());
        proto
    }
}

impl GuildResource for Role {
    type Id = RoleId;
    type Subkey = u64;
    const PREFIX: u8 = 2_u8;

    fn from_key(key: GuildKey) -> Self::Id {
        if let GuildKey::Role(id) = key {
            id
        } else {
            panic!("Invalid GuildKey for channel: {:?}", key);
        }
    }
}

impl ToProto for Role {
    type Proto = CachedRoleProto;
    fn to_proto(&self) -> Self::Proto {
        let mut proto = Self::Proto::new();
        proto.set_role_id(self.id.get());
        proto.set_name(self.name.clone());
        proto.set_position(self.position);
        proto.set_permissions(self.permissions.bits());
        proto
    }
}

pub enum ResumeState {}

impl ResumeState {
    pub async fn save_sessions<C: ConnectionLike>(
        key: &str,
        sessions: HashMap<u64, ResumeSession>,
        redis: &mut C,
    ) -> Result<()> {
        let sessions: Vec<(u64, String)> = sessions
            .into_iter()
            .filter_map(|(shard, session)| serde_json::to_string(&session).map(|s| (shard, s)).ok())
            .collect();
        redis::Cmd::hset_multiple(CacheKey::ResumeState(key.into()), &sessions)
            .query_async::<C, i64>(redis)
            .await?;
        Ok(())
    }

    pub async fn get_sessions<C: ConnectionLike>(
        key: &str,
        redis: &mut C,
    ) -> HashMap<u64, ResumeSession> {
        let sessions = redis::Cmd::hgetall(CacheKey::ResumeState(key.into()))
            .query_async::<C, HashMap<u64, String>>(redis)
            .await;
        if let Ok(sessions) = sessions {
            sessions
                .into_iter()
                .filter_map(|(shard, session)| {
                    serde_json::from_str(&session).map(|s| (shard, s)).ok()
                })
                .collect()
        } else {
            HashMap::new()
        }
    }
}

pub enum MusicQueue {}

impl MusicQueue {
    pub async fn save(guild_id: GuildId, state: MusicStateProto) -> redis::Cmd {
        redis::Cmd::set(CacheKey::MusicQueue(guild_id.get()), Protobuf(state))
    }

    pub async fn clear(guild_id: GuildId) -> redis::Cmd {
        redis::Cmd::del(CacheKey::MusicQueue(guild_id.get()))
    }
}
