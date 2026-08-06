//! ShellState: navigation + data for the main shell (servers, selection,
//! messages, friends pane, DMs). Port of
//! `apps/desktop/src/lib/stores/shell.svelte.ts`. All state lives behind
//! `RwLock`s so the host can read it from Slint bindings and mutate it from
//! async actions.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::api::ApiClient;
use crate::event::{CoreEvent, EventBus};
use crate::protocol::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Servers,
    Friends,
}

pub struct ShellState {
    api: Arc<ApiClient>,
    bus: EventBus,
    /// Monotonic guard: only the newest in-flight load_servers may commit.
    load_seq: AtomicU64,
    pub view: RwLock<View>,
    pub servers: RwLock<Vec<ServerWithChannels>>,
    pub selected_server_id: RwLock<Option<String>>,
    pub selected_channel_id: RwLock<Option<String>>,
    pub messages: RwLock<Vec<TextMessage>>,
    pub loading: RwLock<bool>,
    pub error: RwLock<Option<String>>,
    pub friends: RwLock<Vec<FriendInfo>>,
    pub pending: RwLock<Vec<FriendshipRequest>>,
    pub dm_list: RwLock<Vec<DmSummary>>,
    pub dm_call: RwLock<bool>,
}

impl ShellState {
    pub fn new(api: Arc<ApiClient>, bus: EventBus) -> Arc<Self> {
        Arc::new(Self {
            api,
            bus,
            load_seq: AtomicU64::new(0),
            view: RwLock::new(View::Servers),
            servers: RwLock::new(Vec::new()),
            selected_server_id: RwLock::new(None),
            selected_channel_id: RwLock::new(None),
            messages: RwLock::new(Vec::new()),
            loading: RwLock::new(false),
            error: RwLock::new(None),
            friends: RwLock::new(Vec::new()),
            pending: RwLock::new(Vec::new()),
            dm_list: RwLock::new(Vec::new()),
            dm_call: RwLock::new(false),
        })
    }

    pub fn selected_channel(&self) -> Option<Channel> {
        let channel_id = self.selected_channel_id.read().clone()?;
        if let Some(s) = self
            .servers
            .read()
            .iter()
            .find(|s| Some(s.server.id.as_str()) == self.selected_server_id.read().as_deref())
        {
            if let Some(c) = s.channels.iter().find(|c| c.id == channel_id) {
                return Some(c.clone());
            }
        }
        self.dm_list.read().iter().find(|d| d.channel.id == channel_id).map(|d| d.channel.clone())
    }

    pub fn set_error(&self, msg: String) {
        *self.error.write() = Some(msg.clone());
        self.bus.publish(CoreEvent::Error { message: msg });
    }

    pub async fn load_servers(&self) {
        let seq = self.load_seq.fetch_add(1, Ordering::SeqCst) + 1;
        *self.loading.write() = true;
        match self.api.list_servers().await {
            Ok(mut servers) => {
                if seq != self.load_seq.load(Ordering::SeqCst) {
                    return; // a newer load superseded this one
                }
                // Restore selection if still valid, else pick first server/channel.
                let keep = self.selected_server_id.read().clone();
                if !servers.iter().any(|s| Some(&s.server.id) == keep.as_ref()) {
                    let first = servers.first().cloned();
                    let channel = first
                        .as_ref()
                        .and_then(|s| s.channels.iter().find(|c| c.kind == ChannelKind::Text))
                        .cloned();
                    *self.selected_server_id.write() = first.map(|s| s.server.id);
                    *self.selected_channel_id.write() = channel.map(|c| c.id);
                }
                *self.servers.write() = std::mem::take(&mut servers);
                let should_load = self.selected_channel().map(|c| c.kind != ChannelKind::Voice).unwrap_or(false);
                if should_load {
                    self.load_messages().await;
                }
                self.bus.publish(CoreEvent::ServersLoaded { servers: self.servers.read().clone() });
            }
            Err(e) => self.set_error(e.code),
        }
        *self.loading.write() = false;
    }

    pub async fn select_server(&self, server_id: String) {
        *self.view.write() = View::Servers;
        *self.selected_server_id.write() = Some(server_id.clone());
        // Auto-select the first text channel.
        let channel = self
            .servers
            .read()
            .iter()
            .find(|s| s.server.id == server_id)
            .and_then(|s| s.channels.iter().find(|c| c.kind == ChannelKind::Text))
            .cloned();
        *self.selected_channel_id.write() = channel.as_ref().map(|c| c.id.clone());
        if channel.map(|c| c.kind != ChannelKind::Voice).unwrap_or(false) {
            self.load_messages().await;
        }
    }

    pub async fn select_channel(&self, channel_id: String) {
        *self.selected_channel_id.write() = Some(channel_id.clone());
        let kind = self.selected_channel().map(|c| c.kind);
        if kind != Some(ChannelKind::Voice) {
            self.load_messages().await;
        }
    }

    pub async fn load_messages(&self) {
        let Some(channel_id) = self.selected_channel_id.read().clone() else { return };
        match self.api.list_messages(&channel_id, 50).await {
            Ok(msgs) => {
                *self.messages.write() = msgs;
                self.bus.publish(CoreEvent::MessagesLoaded { channel_id, messages: self.messages.read().clone() });
            }
            Err(e) => self.set_error(e.code),
        }
    }

    pub async fn send_message(&self, content: String) -> Result<(), String> {
        let Some(channel_id) = self.selected_channel_id.read().clone() else {
            return Err("no channel selected".into());
        };
        self.api.post_message(&channel_id, &content).await.map_err(|e| e.code)?;
        self.load_messages().await;
        Ok(())
    }

    pub async fn create_server(&self, name: String) -> Result<(), String> {
        self.api.create_server(&name).await.map_err(|e| e.code)?;
        self.load_servers().await;
        Ok(())
    }

    pub async fn join_server(&self, invite_code: String) -> Result<(), String> {
        self.api.join_server(&invite_code).await.map_err(|e| e.code)?;
        self.load_servers().await;
        Ok(())
    }

    pub async fn create_channel(&self, name: String, kind: ChannelKind) -> Result<(), String> {
        let Some(server_id) = self.selected_server_id.read().clone() else {
            return Err("no server selected".into());
        };
        self.api.create_channel(&server_id, &name, kind).await.map_err(|e| e.code)?;
        self.load_servers().await;
        Ok(())
    }

    pub async fn load_friends(&self) {
        let (friends, pending, dms) = match self.api.get_friends().await {
            Ok(f) => (f.friends, f.pending, self.api.list_dms().await.unwrap_or_default()),
            Err(e) => {
                self.set_error(e.code);
                return;
            }
        };
        *self.friends.write() = friends;
        *self.pending.write() = pending;
        *self.dm_list.write() = dms;
        self.bus.publish(CoreEvent::FriendsLoaded {
            friends: self.friends.read().clone(),
            pending: self.pending.read().clone(),
            dms: self.dm_list.read().clone(),
        });
    }

    pub async fn send_friend_request(&self, username: String) -> Result<(), String> {
        self.api.send_friend_request(&username).await.map_err(|e| e.code)?;
        self.load_friends().await;
        Ok(())
    }

    pub async fn accept_friend(&self, request_id: String) -> Result<(), String> {
        self.api.accept_friend_request(&request_id).await.map_err(|e| e.code)?;
        self.load_friends().await;
        Ok(())
    }

    pub async fn decline_friend(&self, request_id: String) -> Result<(), String> {
        self.api.decline_friend_request(&request_id).await.map_err(|e| e.code)?;
        self.load_friends().await;
        Ok(())
    }

    /// Open (creating if needed) a 1:1 DM with a friend and show it.
    pub async fn open_dm(&self, username: String) -> Result<(), String> {
        let dm = self.api.create_dm(&username).await.map_err(|e| e.code)?;
        self.load_friends().await;
        *self.selected_channel_id.write() = Some(dm.channel.channel.id.clone());
        *self.view.write() = View::Friends;
        self.load_messages().await;
        Ok(())
    }

    pub fn reset(&self) {
        *self.view.write() = View::Servers;
        *self.servers.write() = Vec::new();
        *self.selected_server_id.write() = None;
        *self.selected_channel_id.write() = None;
        *self.messages.write() = Vec::new();
        *self.friends.write() = Vec::new();
        *self.pending.write() = Vec::new();
        *self.dm_list.write() = Vec::new();
        *self.dm_call.write() = false;
        *self.error.write() = None;
    }
}

/// Presence: a friend counts as online if `lastSeen` is fresh (< 5 min).
/// The backend stores ISO 8601 UTC (`toISOString()`, or D1 `strftime('%f')`
/// with 3-6 fractional digits), so parse that (with optional offset) rather
/// than assuming epoch millis.
pub fn is_online(last_seen: &str) -> bool {
    match parse_iso_millis(last_seen) {
        Some(ts) => {
            let now = now_millis();
            ts < now && now - ts < 5 * 60_000
        }
        None => false,
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse `YYYY-MM-DDThh:mm:ss[.fff...][Z|±hh[:]mm]` to Unix epoch millis.
fn parse_iso_millis(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |i: usize, n: usize| -> Option<i64> {
        std::str::from_utf8(&b[i..i + n]).ok()?.parse::<i64>().ok()
    };
    let year = num(0, 4)?;
    let month = num(5, 2)?;
    let day = num(8, 2)?;
    let hour = num(11, 2)?;
    let minute = num(14, 2)?;
    let second = num(17, 2)?;
    // Optional fraction.
    let mut frac_ms: i64 = 0;
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
        let digits = &s[start..i];
        // First 3 digits are millis; pad/truncate.
        let mut d = digits.to_string();
        while d.len() < 3 {
            d.push('0');
        }
        frac_ms = d[..3].parse::<i64>().ok()?;
    }
    // Optional offset: Z or ±hh[:]mm.
    let mut offset_min: i64 = 0;
    if let Some(&c) = b.get(i) {
        if c == b'Z' {
            // UTC
        } else if c == b'+' || c == b'-' {
            let sign = if c == b'-' { -1 } else { 1 };
            let oh = num(i + 1, 2)?;
            let mut om: i64 = 0;
            let mut j = i + 3;
            if b.get(j) == Some(&b':') {
                j += 1;
            }
            if j + 2 <= b.len() {
                om = num(j, 2)?;
            }
            offset_min = sign * (oh * 60 + om);
        } else {
            return None;
        }
    }
    // Days from civil (Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let utc_ms = days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1_000 + frac_ms;
    Some(utc_ms - offset_min * 60_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_parse_utc() {
        let ms = parse_iso_millis("2026-08-05T10:00:00.000Z").unwrap();
        // 2026-08-05T10:00:00Z epoch millis
        assert!(ms > 1_700_000_000_000, "unexpected {ms}");
    }

    #[test]
    fn iso_parse_fraction_and_offset() {
        // Same instant with 6-digit fraction vs offset: both equal.
        let a = parse_iso_millis("2026-08-05T10:00:00.000000Z").unwrap();
        let b = parse_iso_millis("2026-08-05T12:00:00.000+02:00").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn iso_parse_bad() {
        assert!(parse_iso_millis("not-a-date").is_none());
        assert!(parse_iso_millis("").is_none());
    }
}

