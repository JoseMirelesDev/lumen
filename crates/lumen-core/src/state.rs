//! ShellState: navigation + data for the main shell (servers, selection,
//! messages, friends pane, DMs). Port of
//! `apps/desktop/src/lib/stores/shell.svelte.ts`. All state lives behind
//! `RwLock`s so the host can read it from Slint bindings and mutate it from
//! async actions.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use crate::api::ApiClient;
use crate::event::{CoreEvent, EventBus};
use crate::presence::{PresenceClient, PresenceOut};
use crate::protocol::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Servers,
    Friends,
}

/// Current UTC time as `YYYY-MM-DDTHH:MM:SS.mmmZ` (matches the backend's
/// D1/`toISOString()` format the client parses).
pub fn now_iso() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let hour = rem / 3_600_000;
    let minute = (rem % 3_600_000) / 60_000;
    let second = (rem % 60_000) / 1000;
    let millis = rem % 1000;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Days since 1970-01-01 → (year, month, day) (Hinnant's civil algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// A friend's live presence (from the presence WS).
#[derive(Debug, Clone)]
pub struct FriendPresence {
    pub username: String,
    pub status: PresenceV2Status,
}

/// Presence state fed by the presence WS (protocol/presence-v2.md).
#[derive(Default)]
pub struct PresenceState {
    pub online_friends: RwLock<std::collections::HashMap<String, FriendPresence>>,
    pub server_presence: RwLock<std::collections::HashMap<String, ServerPresence>>,
}

/// A chat message awaiting its ACK (ADR-005): retransmitted every 3s until
/// confirmed or the attempt budget runs out.
#[derive(Debug, Clone)]
pub struct PendingSend {
    pub channel_id: String,
    pub server_id: String,
    pub content: String,
    pub sent_at: Instant,
    pub attempts: u8,
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
    // Fase 3 — presence + real-time chat.
    pub presence_client: PresenceClient,
    pub presence: PresenceState,
    pub pending_acks: RwLock<std::collections::HashMap<String, PendingSend>>,
}

impl ShellState {
    pub fn new(api: Arc<ApiClient>, bus: EventBus) -> Arc<Self> {
        let presence_client = PresenceClient::new(api.clone(), bus.clone());
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
            presence_client,
            presence: PresenceState::default(),
            pending_acks: RwLock::new(std::collections::HashMap::new()),
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
        self.send_message_with(content, None, None).await
    }

    /// Send with optional reply-to (6.2) and attachment URL (6.4).
    pub async fn send_message_with(
        &self,
        content: String,
        reply_to: Option<String>,
        attachment_url: Option<String>,
    ) -> Result<(), String> {
        let Some(channel_id) = self.selected_channel_id.read().clone() else {
            return Err("no channel selected".into());
        };
        let server_id = self.selected_server_id.read().clone().unwrap_or_default();
        let client_id = format!("{:016x}", rand::random::<u64>());
        self.presence_client.send(PresenceOut::Chat {
            channel_id: channel_id.clone(),
            server_id: server_id.clone(),
            content: content.clone(),
            client_id: client_id.clone(),
            reply_to: reply_to.clone(),
            attachment_url: attachment_url.clone(),
        });
        self.pending_acks.write().insert(
            client_id,
            PendingSend { channel_id, server_id, content, sent_at: Instant::now(), attempts: 0 },
        );
        let _ = (reply_to, attachment_url);
        Ok(())
    }

    // -- Fase 3: presence + real-time chat --------------------------------

    /// Open the presence socket with the current server/friend membership and
    /// the authenticated username. Call after login/load_servers/load_friends;
    /// the socket reconnects internally with backoff.
    /// Open the presence socket (backend resolves membership from the JWT).
    pub async fn connect_presence(&self) {
        self.presence_client.connect().await;
    }

    pub async fn disconnect_presence(&self) {
        self.presence_client.disconnect().await;
        self.pending_acks.write().clear();
    }

    /// Merge the `ready` snapshot into the presence state.
    pub fn apply_presence_ready(
        &self,
        online_friends: Vec<OnlineFriendLite>,
        servers: Vec<ServerPresence>,
    ) {
        {
            let mut friends = self.presence.online_friends.write();
            friends.clear();
            for f in &online_friends {
                friends.insert(
                    f.user_id.clone(),
                    FriendPresence { username: f.username.clone(), status: f.status },
                );
            }
        }
        {
            let mut map = self.presence.server_presence.write();
            map.clear();
            for s in &servers {
                map.insert(s.server_id.clone(), s.clone());
            }
        }
        // NOTE: no re-publish here. The event already arrived on the bus
        // (presence.rs → publish_event); re-publishing it from the apply step
        // re-fed the listener: bus → on_core_event → apply_presence_ready →
        // publish(PresenceReady) → bus → … an infinite in-process loop that
        // drowned the UI thread in push_shell (CPU → 198%). The listener
        // still pushes after applying; other subscribers get the original
        // publish. Keep apply_* as pure state mutation.
    }

    pub fn apply_friend_online(&self, user_id: &str, username: &str) {
        self.presence.online_friends.write().insert(
            user_id.to_string(),
            FriendPresence { username: username.to_string(), status: PresenceV2Status::Online },
        );
        // No re-publish (see apply_presence_ready note): the original event
        // already came through the bus from presence.rs.
    }

    pub fn apply_friend_offline(&self, user_id: &str) {
        self.presence.online_friends.write().remove(user_id);
        // No re-publish (see apply_presence_ready note).
    }

    pub fn apply_friend_status(&self, user_id: &str, status: PresenceV2Status) {
        if let Some(f) = self.presence.online_friends.write().get_mut(user_id) {
            f.status = status;
        }
        // No re-publish (see apply_presence_ready note).
    }

    pub fn apply_member_online(&self, server_id: &str, user_id: &str, username: &str) {
        {
            let mut map = self.presence.server_presence.write();
            if let Some(sp) = map.get_mut(server_id) {
                if !sp.online_members.iter().any(|m| m.user_id == user_id) {
                    sp.online_members.push(PeerLite { user_id: user_id.to_string(), username: username.to_string() });
                }
            }
        }
        // No re-publish (see apply_presence_ready note).
    }

    pub fn apply_member_offline(&self, server_id: &str, user_id: &str) {
        {
            let mut map = self.presence.server_presence.write();
            if let Some(sp) = map.get_mut(server_id) {
                sp.online_members.retain(|m| m.user_id != user_id);
            }
        }
        // No re-publish (see apply_presence_ready note).
    }

    /// A chat-edit invalidation: patch the local copy if present.
    pub fn apply_chat_edited(&self, channel_id: &str, id: &str, content: &str, edited_at: &str) {
        {
            let mut msgs = self.messages.write();
            if let Some(m) = msgs.iter_mut().find(|m| m.id == id && m.channel_id == channel_id) {
                m.content = content.to_string();
                m.edited_at = Some(edited_at.to_string());
            }
        }
        // No re-publish (see apply_presence_ready note); the listener pushes
        // the UI after this returns.
    }

    pub fn apply_chat_deleted(&self, channel_id: &str, id: &str) {
        {
            let mut msgs = self.messages.write();
            if let Some(m) = msgs.iter_mut().find(|m| m.id == id && m.channel_id == channel_id) {
                m.deleted_at = Some(now_iso());
            }
        }
        // No re-publish (see apply_presence_ready note).
    }

    pub fn apply_voice_occupancy(&self, server_id: &str, channel_id: &str, peers: Vec<PeerLite>) {
        if let Some(s) = self.presence.server_presence.write().get_mut(server_id) {
            if let Some(vc) = s.voice_channels.iter_mut().find(|v| v.channel_id == channel_id) {
                vc.peers = peers;
            }
        }
        // No re-publish (see apply_presence_ready note).
    }

    /// A real-time chat frame arrived: append when it belongs to the channel
    /// currently open (the UI only shows the selected channel).
    pub fn on_realtime_message(&self, channel_id: &str, message: BufferedMessage) {
        if self.selected_channel_id.read().as_deref() != Some(channel_id) {
            return;
        }
        let mut msgs = self.messages.write();
        msgs.push(TextMessage {
            id: message.id,
            channel_id: channel_id.to_string(),
            author_id: message.author_id,
            author_name: message.author_name,
            content: message.content,
            created_at: message.created_at,
            edited_at: message.edited_at,
            deleted_at: message.deleted_at,
            reply_to: message.reply_to,
        });
        self.bus.publish(CoreEvent::MessagesLoaded { channel_id: channel_id.to_string(), messages: msgs.clone() });
    }

    pub fn on_chat_ack(self: &Arc<Self>, client_id: &str) {
        self.pending_acks.write().remove(client_id);
        // No reload here: the listener owns the post-ack reload+push so the
        // confirmed message renders with its real id + server timestamp
        // (see controller.rs ChatAck arm).
    }

    /// Retransmission worker (ADR-005): every 3s re-send pending chats older
    /// than 3s, up to 3 attempts; on the 4th, drop with an error event.
    pub fn spawn_retransmitter(self: &Arc<Self>, rt: tokio::runtime::Handle) {
        let this = Arc::clone(self);
        rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            loop {
                tick.tick().await;
                let now = Instant::now();
                let expired: Vec<(String, PendingSend)> = {
                    let mut acks = this.pending_acks.write();
                    let mut keep = std::collections::HashMap::new();
                    let mut expired = Vec::new();
                    for (client_id, p) in acks.drain() {
                        if now.duration_since(p.sent_at) >= Duration::from_secs(3) {
                            expired.push((client_id, p));
                        } else {
                            keep.insert(client_id, p);
                        }
                    }
                    *acks = keep;
                    expired
                };
                for (client_id, p) in expired {
                    if p.attempts >= 3 {
                        this.bus.publish(CoreEvent::ChatError { client_id, code: "send_timeout".into() });
                        this.set_error("no se pudo enviar el mensaje".into());
                    } else {
                        let mut p = p;
                        p.attempts += 1;
                        p.sent_at = Instant::now();
                        this.presence_client.send(PresenceOut::Chat {
                            channel_id: p.channel_id.clone(),
                            server_id: p.server_id.clone(),
                            content: p.content.clone(),
                            client_id: client_id.clone(),
                            reply_to: None,
                            attachment_url: None,
                        });
                        this.pending_acks.write().insert(client_id, p);
                    }
                }
            }
        });
    }

    /// Send typing for the selected channel (rate-limited server-side).
    pub fn send_typing(&self) {
        let (Some(channel_id), Some(server_id)) = (
            self.selected_channel_id.read().clone(),
            self.selected_server_id.read().clone(),
        ) else {
            return;
        };
        self.presence_client.send(PresenceOut::Typing { channel_id, server_id });
    }

    pub fn presence_voice_join(&self, channel_id: String, server_id: String) {
        self.presence_client.send(PresenceOut::VoiceJoin { channel_id, server_id });
    }

    pub fn presence_voice_leave(&self) {
        self.presence_client.send(PresenceOut::VoiceLeave);
    }

    /// Edit/delete via the presence WS (ADR-0010) with pending ACK handling.
    pub fn edit_message_ws(&self, channel_id: String, server_id: String, message_id: String, content: String) {
        let client_id = format!("{:016x}", rand::random::<u64>());
        self.presence_client.send(PresenceOut::ChatEdit { channel_id, server_id, message_id, content, client_id });
    }

    pub fn delete_message_ws(&self, channel_id: String, server_id: String, message_id: String) {
        let client_id = format!("{:016x}", rand::random::<u64>());
        self.presence_client.send(PresenceOut::ChatDelete { channel_id, server_id, message_id, client_id });
    }

    pub fn subscribe_channel(&self, channel_id: String) {
        self.presence_client.send(PresenceOut::Subscribe(channel_id));
    }

    pub fn unsubscribe_channel(&self, channel_id: String) {
        self.presence_client.send(PresenceOut::Unsubscribe(channel_id));
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

    /// Refresco ligero: amigos + solicitudes pendientes, SIN DMs. El refresco
    /// periódico de la vista Friends solo necesita detectar solicitudes
    /// entrantes; omitir list_dms ahorra un request por tick.
    pub async fn refresh_friends(&self) {
        match self.api.get_friends().await {
            Ok(f) => {
                *self.friends.write() = f.friends;
                *self.pending.write() = f.pending;
                self.bus.publish(CoreEvent::FriendsLoaded {
                    friends: self.friends.read().clone(),
                    pending: self.pending.read().clone(),
                    dms: self.dm_list.read().clone(),
                });
            }
            Err(e) => self.set_error(e.code),
        }
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

    // ------------------------------------------------------------------
    // Fase 2 — CRUD actions
    // ------------------------------------------------------------------

    pub async fn update_server(&self, server_id: String, name: String) -> Result<(), String> {
        self.api.update_server(&server_id, Some(&name)).await.map_err(|e| e.code)?;
        self.load_servers().await;
        Ok(())
    }

    pub async fn delete_server(&self, server_id: String) -> Result<(), String> {
        self.api.delete_server(&server_id).await.map_err(|e| e.code)?;
        self.load_servers().await;
        Ok(())
    }

    pub async fn leave_server(&self, server_id: String) -> Result<(), String> {
        self.api.leave_server(&server_id).await.map_err(|e| e.code)?;
        self.load_servers().await;
        Ok(())
    }

    pub async fn regenerate_invite(&self, server_id: String) -> Result<String, String> {
        let code = self.api.regenerate_invite(&server_id).await.map_err(|e| e.code)?;
        self.load_servers().await; // refresh the invite code in the model
        Ok(code)
    }

    pub async fn kick_member(&self, server_id: String, user_id: String) -> Result<(), String> {
        self.api.kick_member(&server_id, &user_id).await.map_err(|e| e.code)?;
        Ok(())
    }

    pub async fn update_channel(&self, channel_id: String, patch: serde_json::Value) -> Result<(), String> {
        self.api.update_channel(&channel_id, &patch).await.map_err(|e| e.code)?;
        self.load_servers().await;
        Ok(())
    }

    pub async fn delete_channel(&self, channel_id: String) -> Result<(), String> {
        self.api.delete_channel(&channel_id).await.map_err(|e| e.code)?;
        if self.selected_channel_id.read().as_deref() == Some(channel_id.as_str()) {
            *self.selected_channel_id.write() = None;
        }
        self.load_servers().await;
        Ok(())
    }

    pub async fn edit_message(&self, message_id: String, content: String) -> Result<(), String> {
        self.api.edit_message(&message_id, &content).await.map_err(|e| e.code)?;
        self.load_messages().await;
        Ok(())
    }

    pub async fn delete_message(&self, message_id: String) -> Result<(), String> {
        self.api.delete_message(&message_id).await.map_err(|e| e.code)?;
        self.load_messages().await;
        Ok(())
    }

    pub async fn remove_friend(&self, user_id: String) -> Result<(), String> {
        self.api.remove_friend(&user_id).await.map_err(|e| e.code)?;
        self.load_friends().await;
        Ok(())
    }

    pub async fn delete_dm(&self, channel_id: String) -> Result<(), String> {
        self.api.delete_dm(&channel_id).await.map_err(|e| e.code)?;
        self.load_friends().await;
        Ok(())
    }

    pub async fn change_password(&self, current: String, new: String) -> Result<(), String> {
        self.api.change_password(&current, &new).await.map_err(|e| e.code)?;
        Ok(())
    }

    pub async fn delete_account(&self) -> Result<(), String> {
        self.api.delete_account().await.map_err(|e| e.code)?;
        Ok(())
    }

    // Fase 5 — moderación
    pub async fn ban_member(&self, server_id: String, user_id: String, reason: Option<String>) -> Result<(), String> {
        self.api.ban_member(&server_id, &user_id, reason.as_deref()).await.map_err(|e| e.code)?;
        Ok(())
    }

    pub async fn unban_member(&self, server_id: String, user_id: String) -> Result<(), String> {
        self.api.unban_member(&server_id, &user_id).await.map_err(|e| e.code)?;
        Ok(())
    }

    pub async fn block_user(&self, user_id: String) -> Result<(), String> {
        self.api.block_user(&user_id).await.map_err(|e| e.code)?;
        Ok(())
    }

    pub async fn unblock_user(&self, user_id: String) -> Result<(), String> {
        self.api.unblock_user(&user_id).await.map_err(|e| e.code)?;
        Ok(())
    }

    pub async fn report(&self, target_type: String, target_id: String, reason: Option<String>) -> Result<(), String> {
        self.api.report(&target_type, &target_id, reason.as_deref()).await.map_err(|e| e.code)?;
        Ok(())
    }

    pub async fn transfer_server(&self, server_id: String, user_id: String) -> Result<(), String> {
        self.api.transfer_server(&server_id, &user_id).await.map_err(|e| e.code)?;
        self.load_servers().await;
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

