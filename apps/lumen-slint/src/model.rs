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

/// Silhouette palette skin derived from a name — stable per user, one of the
/// 7 pre-recolored placeholder skins (designs/vox-mockups palette). Real
/// multi-color sprites replace the silhouettes later; this mapping stays.
pub const SKINS: [&str; 7] = ["ivory", "gold", "crimson", "teal", "indigo", "stone", "bronze"];

pub fn skin_for(name: &str) -> SharedString {
    let mut h: u64 = 1469598103934665603;
    for b in name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    SKINS[(h % SKINS.len() as u64) as usize].into()
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
            skin: skin_for(&s.server.name),
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

pub fn channels_model(
    channels: &[Channel],
    selected: &Option<String>,
    peers_by_channel: &std::collections::HashMap<String, usize>,
    joined_voice: &Option<String>,
) -> ModelRc<ChannelItem> {
    let items: Vec<ChannelItem> = channels
        .iter()
        .map(|c| ChannelItem {
            id: c.id.clone().into(),
            name: c.name.clone().into(),
            kind: c.kind.as_str().into(),
            active: selected.as_deref() == Some(c.id.as_str()),
            peer_count: peers_by_channel.get(&c.id).copied().unwrap_or(0) as i32,
            joined: joined_voice.as_deref() == Some(c.id.as_str()),
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// Split channels into two models (text / voice). Slint 1.17 does NOT remove
/// `visible: false` elements from a layout — invisible rows still occupy
/// their slot — so filtering must happen here, not via `visible:` in .slint.
pub fn channels_model_filtered(
    channels: &[Channel],
    selected: &Option<String>,
    want: &str,
    peers_by_channel: &std::collections::HashMap<String, usize>,
    joined_voice: &Option<String>,
) -> ModelRc<ChannelItem> {
    let items: Vec<ChannelItem> = channels
        .iter()
        .filter(|c| c.kind.as_str() == want)
        .map(|c| ChannelItem {
            id: c.id.clone().into(),
            name: c.name.clone().into(),
            kind: c.kind.as_str().into(),
            active: selected.as_deref() == Some(c.id.as_str()),
            peer_count: peers_by_channel.get(&c.id).copied().unwrap_or(0) as i32,
            joined: joined_voice.as_deref() == Some(c.id.as_str()),
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// Online members of a server, for the presence strip (Fase 3).
pub fn members_model(members: &[lumen_core::PeerLite]) -> ModelRc<MemberItem> {
    let items: Vec<MemberItem> = members
        .iter()
        .map(|m| MemberItem {
            id: m.user_id.clone().into(),
            username: m.username.clone().into(),
            online: true,
            skin: skin_for(&m.username),
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

/// First URL in `content`: starts with "http://" or "https://" and runs to
/// the next whitespace (or end of string), with common trailing sentence
/// punctuation stripped so "see https://x.dev." opens x.dev, not x.dev.
/// Returns "" when the content has no URL.
///
/// Hardened: the candidate is parsed with `url::Url` and rejected unless it is
/// a well-formed http(s) URL with a non-empty host and NO userinfo
/// (`https://user:pass@host` — embeds credentials and enables display
/// spoofing) and NO control characters. The URL is only *detected* here; the
/// SSRF guard (public-host check) still runs on any actual fetch.
pub fn detect_link(content: &str) -> String {
    const SCHEMES: [&str; 2] = ["http://", "https://"];
    let mut start: Option<usize> = None;
    for scheme in SCHEMES {
        if let Some(i) = content.find(scheme) {
            start = Some(match start {
                Some(prev) => prev.min(i),
                None => i,
            });
        }
    }
    let Some(start) = start else { return String::new() };
    let rest = &content[start..];
    let end = rest
        .char_indices()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    let candidate = rest[..end]
        .trim_end_matches(|c| matches!(c, '.' | ',' | ';' | '!' | '?' | ')' | ']' | '}' | '>' | '"' | '\''))
        .to_string();

    // Reject anything that isn't a clean http(s) URL: bad scheme, empty host,
    // userinfo (credentials in the URL), or control characters (which can
    // break out of the display / terminal).
    let Ok(parsed) = url::Url::parse(&candidate) else {
        return String::new();
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return String::new();
    }
    // The `url` crate is lenient: "https:///path" parses with host "path".
    // Detect the real no-host form (scheme + ":///" = empty authority) and
    // reject it; a genuine empty host is never valid for our purposes.
    if candidate.contains(":///") {
        return String::new();
    }
    if parsed.host_str().unwrap_or_default().is_empty() {
        return String::new();
    }
    if parsed.username() != "" || parsed.password().is_some() {
        return String::new();
    }
    if candidate.chars().any(|c| c.is_control()) {
        return String::new();
    }
    candidate
}

pub fn messages_model(msgs: &[TextMessage], self_user_id: &str) -> ModelRc<MessageItem> {
    let items: Vec<MessageItem> = msgs
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let first_in_run = i == 0 || msgs[i - 1].author_id != m.author_id;
            let link_url = detect_link(&m.content);
            MessageItem {
                id: m.id.clone().into(),
                author: m.author_name.clone().into(),
                time: hhmm(&m.created_at),
                content: m.content.clone().into(),
                mine: m.author_id == self_user_id,
                skin: skin_for(&m.author_name),
                first_in_run,
                // Title is filled asynchronously by the controller once it
                // resolves the URL (see UiController::resolve_link_previews).
                link_url: link_url.into(),
                link_title: "".into(),
                edited: m.edited_at.is_some(),
                deleted: m.deleted_at.is_some(),
                reply_to: m.reply_to.clone().unwrap_or_default().into(),
            }
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

pub fn friend_model(f: &FriendInfo) -> FriendItem {
    FriendItem {
        id: f.user.id.clone().into(),
        username: f.user.username.clone().into(),
        shared: f.shared_servers as i32,
        skin: skin_for(&f.user.username),
    }
}

/// Online/offline friends. When the presence socket is connected the
/// real-time presence set wins; otherwise fall back to the last_seen
/// heuristic (is_online). `online_ids` is the presence hub's online set.
pub fn friends_online_model(
    friends: &[FriendInfo],
    online_ids: &std::collections::HashSet<String>,
) -> ModelRc<FriendItem> {
    let items: Vec<FriendItem> = friends
        .iter()
        .filter(|f| {
            if !online_ids.is_empty() {
                online_ids.contains(&f.user.id)
            } else {
                lumen_core::is_online(&f.user.last_seen)
            }
        })
        .map(friend_model)
        .collect();
    ModelRc::new(VecModel::from(items))
}

pub fn friends_offline_model(
    friends: &[FriendInfo],
    online_ids: &std::collections::HashSet<String>,
) -> ModelRc<FriendItem> {
    let items: Vec<FriendItem> = friends
        .iter()
        .filter(|f| {
            if !online_ids.is_empty() {
                !online_ids.contains(&f.user.id)
            } else {
                !lumen_core::is_online(&f.user.last_seen)
            }
        })
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
        .map(|d| DmItem {
            id: d.channel.id.clone().into(),
            username: d.other_username.clone().into(),
            skin: skin_for(&d.other_username),
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// Toast stack (transient notifications) — direct mapping, no transforms.
pub fn toasts_model(toasts: &[ToastItem]) -> ModelRc<ToastItem> {
    ModelRc::new(VecModel::from(toasts.to_vec()))
}

#[cfg(test)]
mod detect_link_tests {
    use super::detect_link;

    #[test]
    fn detects_plain_url() {
        assert_eq!(detect_link("see https://example.com"), "https://example.com");
        assert_eq!(detect_link("http://example.com/path?q=1"), "http://example.com/path?q=1");
    }

    #[test]
    fn strips_trailing_punctuation() {
        assert_eq!(detect_link("go to https://example.com."), "https://example.com");
        assert_eq!(detect_link("(https://example.com)"), "https://example.com");
        assert_eq!(detect_link("see https://x.dev, ok"), "https://x.dev");
    }

    #[test]
    fn picks_earliest_of_two() {
        assert_eq!(
            detect_link("a https://first.com b http://second.com"),
            "https://first.com"
        );
    }

    #[test]
    fn returns_empty_without_url() {
        assert_eq!(detect_link("no links here"), "");
        assert_eq!(detect_link(""), "");
    }

    #[test]
    fn rejects_userinfo_credentials() {
        // Credentials in the URL: display spoofing + would fetch with them.
        assert_eq!(detect_link("https://user:pass@example.com"), "");
        assert_eq!(detect_link("https://user@example.com"), "");
    }

    #[test]
    fn rejects_malformed_and_control_chars() {
        // Control characters can break display/terminal output.
        assert_eq!(detect_link("https://example.com/\u{0}"), "");
        // No host.
        assert_eq!(detect_link("https:///path"), "");
    }

    #[test]
    fn keeps_valid_ports_and_ip_hosts() {
        assert_eq!(detect_link("http://127.0.0.1:8080/x"), "http://127.0.0.1:8080/x");
        assert_eq!(detect_link("https://[::1]/x"), "https://[::1]/x");
    }
}
