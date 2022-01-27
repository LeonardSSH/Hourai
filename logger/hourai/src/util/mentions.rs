use crate::models::id::{marker::*, Id};
use regex::Regex;
use std::str::FromStr;

lazy_static! {
    static ref USER_MENTION_REGEX: Regex = Regex::new(r"<@!?(\d+)>").unwrap();
    static ref ROLE_MENTION_REGEX: Regex = Regex::new(r"<@&(\d+)>").unwrap();
    static ref CHANNEL_MENTION_REGEX: Regex = Regex::new(r"<@#(\d+)>").unwrap();
}

pub fn get_user_mention_ids(text: &str) -> impl Iterator<Item = Id<UserMarker>> + '_ {
    USER_MENTION_REGEX
        .find_iter(text)
        .filter_map(|hit| u64::from_str(hit.as_str()).ok())
        .map(Id::new)
}

pub fn get_role_mention_ids(text: &str) -> impl Iterator<Item = Id<RoleMarker>> + '_ {
    ROLE_MENTION_REGEX
        .find_iter(text)
        .filter_map(|hit| u64::from_str(hit.as_str()).ok())
        .map(Id::new)
}

pub fn get_channel_mention_ids(text: &str) -> impl Iterator<Item = Id<ChannelMarker>> + '_ {
    CHANNEL_MENTION_REGEX
        .find_iter(text)
        .filter_map(|hit| u64::from_str(hit.as_str()).ok())
        .map(Id::new)
}
