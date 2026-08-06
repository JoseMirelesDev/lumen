//! Rust ↔ Slint data mapping: lumen-core types → the generated Slint structs.
//! The Slint structs come from ui/types.slint + ui/app.slint.

slint::include_modules!();

use lumen_core::{
    Channel, DmSummary, FriendInfo, FriendshipRequest, ServerWithChannels, TextMessage, User,
};
use slint::{ModelRc, SharedString, VecModel};

pub fn first_char_upper(s: &str) -> SharedString {
    s.chars()
        .next()
        .map(|c| c.to_uppercase().collect::<String>())
        .unwrap_or_default()
        .into()
}

pub fn user_initial(u: &User) -> SharedString {
    first_char_upper(&u.username)
}

pub fn servers_model(servers: &[ServerWithChannels], selected: &Option<String>) -> ModelRc<ServerItem> {
    let items: Vec<ServerItem> = servers
        .iter()
        .map(|s| ServerItem {
            id: s.server.id.clone().into(),
            label: first_char_upper(&s.server.name),
            active: selected.as_deref() == Some(s.server.id.as_str()),
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

pub fn channels_model(channels: &[Channel], selected: &Option<String>) -> ModelRc<ChannelItem> {
    let items: Vec<ChannelItem> = channels
        .iter()
        .map(|c| ChannelItem {
            id: c.id.clone().into(),
            name: c.name.clone().into(),
            kind: c.kind.as_str().into(),
            active: selected.as_deref() == Some(c.id.as_str()),
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// "HH:MM" from an ISO-8601 timestamp ("2026-08-05T10:00:00.000Z" → "10:00").
fn hhmm(iso: &str) -> SharedString {
    if iso.len() >= 16 {
        iso[11..16].to_string().into()
    } else {
        iso.into()
    }
}

pub fn messages_model(msgs: &[TextMessage], self_user_id: &str) -> ModelRc<MessageItem> {
    let items: Vec<MessageItem> = msgs
        .iter()
        .map(|m| MessageItem {
            id: m.id.clone().into(),
            author: m.author_name.clone().into(),
            time: hhmm(&m.created_at),
            content: m.content.clone().into(),
            mine: m.author_id == self_user_id,
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

pub fn friend_model(f: &FriendInfo) -> FriendItem {
    FriendItem { username: f.user.username.clone().into(), shared: f.shared_servers as i32 }
}

pub fn friends_online_model(friends: &[FriendInfo]) -> ModelRc<FriendItem> {
    let items: Vec<FriendItem> = friends
        .iter()
        .filter(|f| lumen_core::is_online(&f.user.last_seen))
        .map(friend_model)
        .collect();
    ModelRc::new(VecModel::from(items))
}

pub fn friends_offline_model(friends: &[FriendInfo]) -> ModelRc<FriendItem> {
    let items: Vec<FriendItem> = friends
        .iter()
        .filter(|f| !lumen_core::is_online(&f.user.last_seen))
        .map(friend_model)
        .collect();
    ModelRc::new(VecModel::from(items))
}

pub fn requests_model(requests: &[FriendshipRequest]) -> ModelRc<FriendRequestItem> {
    let items: Vec<FriendRequestItem> = requests
        .iter()
        .filter(|r| r.direction == lumen_core::RequestDirection::Incoming)
        .map(|r| FriendRequestItem {
            id: r.id.clone().into(),
            username: r.user.username.clone().into(),
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

pub fn dms_model(dms: &[DmSummary]) -> ModelRc<DmItem> {
    let items: Vec<DmItem> = dms
        .iter()
        .map(|d| DmItem { id: d.channel.id.clone().into(), username: d.other_username.clone().into() })
        .collect();
    ModelRc::new(VecModel::from(items))
}
