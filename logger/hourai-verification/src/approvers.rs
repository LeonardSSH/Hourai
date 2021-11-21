use super::{context, verifier::*};
use anyhow::Result;
use async_trait::async_trait;
use hourai::cache::InMemoryCache;
use hourai::models::id::UserId;
use hourai::models::user::{PremiumType, User, UserFlags};
use std::collections::HashSet;

const VERIFIED_FEATURE: &str = "VERIFIED";

struct DistinguishedUserVerifier(InMemoryCache);

#[async_trait]
impl Verifier for DistinguishedUserVerifier {
    async fn verify(&self, ctx: &mut context::VerificationContext) -> Result<()> {
        let flags = ctx.member().user.flags.unwrap_or_else(UserFlags::empty);
        if flags.contains(UserFlags::STAFF) {
            ctx.add_approval_reason("User is Discord Staff.");
        }
        if flags.contains(UserFlags::PARTNER) {
            ctx.add_approval_reason("User is a Discord Partner.");
        }
        if flags.contains(UserFlags::VERIFIED_DEVELOPER) {
            ctx.add_approval_reason("User is a verified bot developer.");
        }
        // TODO(james7123): This will not scale to multiple processes
        //let member_id = ctx.member().user.id;
        //for guild_id in self.0.guilds() {
        //if let Some(guild) = self.0.guild(guild_id) {
        //let verified = guild
        //.features
        //.iter()
        //.any(|feat| feat.as_ref() == VERIFIED_FEATURE);
        //if guild.owner_id == member_id && verified {
        //ctx.add_approval_reason("User is the owner of a verified server.");
        //}
        //}
        //}
        Ok(())
    }
}

pub fn user_has_nitro(user: &User) -> bool {
    let premium = user
        .premium_type
        .map(|premium| premium != PremiumType::None)
        .unwrap_or(false);
    let flag = user
        .public_flags
        .map(|f| f.contains(UserFlags::PREMIUM_EARLY_SUPPORTER))
        .unwrap_or(false);
    let has_banner = user.banner.is_some();
    let animated = user
        .avatar
        .as_ref()
        .map(|a| a.starts_with("a_"))
        .unwrap_or(false);
    premium || has_banner || flag || animated
}

pub(super) fn nitro() -> BoxedVerifier {
    GenericVerifier::new_approver(
        "User currently has or has had Nitro. Probably not a user bot.",
        |ctx| Ok(user_has_nitro(&ctx.member().user)),
    )
}

pub(super) fn bot_owners(owners: impl IntoIterator<Item = UserId>) -> BoxedVerifier {
    let owner_ids: HashSet<UserId> = owners.into_iter().collect();
    GenericVerifier::new_approver("User is an owner of this bot.", move |ctx| {
        Ok(owner_ids.contains(&ctx.member().user.id))
    })
}

pub(super) fn bot() -> BoxedVerifier {
    GenericVerifier::new_approver(
        "User is an OAuth2 bot that can only be manually added by moderators.",
        |ctx| Ok(ctx.member().user.bot),
    )
}

pub(super) fn distinguished_user(cache: InMemoryCache) -> BoxedVerifier {
    Box::new(DistinguishedUserVerifier(cache))
}
