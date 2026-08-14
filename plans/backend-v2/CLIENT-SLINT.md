# CLIENT-SLINT.md — Integracion del cliente (Fases 1-6)

Status: **GUIA DE IMPLEMENTACION** — complementa los `phases/*.md` desde
el lado del cliente. Cada seccion mapea un cambio de backend al trabajo
concreto en los crates Rust y la UI Slint.

## 0. Arquitectura del cliente (como esta hoy)

```
apps/lumen-slint/
  src/main.rs          ← arranque, registro de protocolo lumen:// (Fase 4)
  src/controller.rs    ← UiController: orquesta servicios + push_shell
  src/ctrl/
    auth.rs            ← login/register/logout/session bootstrap
    shell.rs           ← servers, channels, friends, DMs, invite
    chat.rs            ← messages, composer, links, previews
  src/voice.rs         ← VoiceController (P2P voice)
  src/model.rs         ← lumen-core types → Slint structs (ModelRc/VecModel)
  src/sound.rs         ← sfx
  ui/                  ← .slint (types.slint, app.slint, ...)
    types.slint        ← structs compartidas
    app.slint          ← layout principal

crates/lumen-core/
  src/api.rs           ← ApiClient REST (reqwest, token slot)
  src/auth.rs          ← AuthService (login/register/logout)
  src/state.rs         ← ShellState (RwLock por dominio)
  src/event.rs         ← EventBus: tokio broadcast de CoreEvent
  src/protocol.rs      ← tipos del protocolo (mirror de @lumen/protocol)

crates/lumen-voice/
  src/client.rs        ← VoiceClient/VoiceSession (WebRTC)
  src/signaling.rs     ← SignalingClient (WS al ChannelDO)
  src/event.rs         ← VoiceEvent
```

**Flujo de datos actual:**

```
API/WS ──► lumen-core ──► CoreEvent (broadcast)
              │
              ▼
        UiController (subscrito al bus)
              │
              ├─► ctrl::* (mutan ShellState)
              │
              └─► push_shell() → escribe propiedades Slint
                    │
                    ▼
              model.rs (Rust types → Slint structs)
                    │
                    ▼
              ui/*.slint (render)
```

**Regla de integracion:** TODA la logica nueva vive en lumen-core
(estado + eventos). El UiController solo traduce CoreEvent → propiedades
Slint. Los .slint solo renderizan. No poner logica en la UI.

---

## Fase 1 — Seguridad (cliente)

### 1.1 lumen-core/src/auth.rs — refresh tokens

```rust
pub struct AuthService {
    api: Arc<ApiClient>,
    bus: EventBus,
    // nuevo:
    refresh_token: RwLock<Option<String>>,
}

impl AuthService {
    // login/register ahora guardan refresh_token ademas del token
    pub async fn login(&self, username: &str, password: &str) -> Result<User, ApiError> {
        let res = self.api.post::<AuthResponse>("/api/auth/login",
            &serde_json::json!({ "username": username, "password": password })).await?;
        *self.token.write() = Some(res.token);
        *self.refresh_token.write() = Some(res.refreshToken);
        self.bus.publish(CoreEvent::Authenticated { user: res.user.clone() });
        Ok(res.user)
    }

    /// Intenta refrescar el access token; devuelve false si el refresh expiro.
    pub async fn try_refresh(&self) -> bool {
        let rt = self.refresh_token.read().clone();
        let Some(rt) = rt else { return false };
        match self.api.post::<RefreshResponse>("/api/auth/refresh",
                &serde_json::json!({ "refreshToken": rt })).await {
            Ok(res) => {
                *self.token.write() = Some(res.token);
                if let Some(new_rt) = res.refreshToken {
                    *self.refresh_token.write() = Some(new_rt);
                }
                true
            }
            Err(_) => {
                *self.refresh_token.write() = None;
                false
            }
        }
    }

    pub async fn logout(&self) {
        if let Some(rt) = self.refresh_token.read().clone() {
            let _ = self.api.post::<serde_json::Value>("/api/auth/logout",
                &serde_json::json!({ "refreshToken": rt })).await;
        }
        *self.token.write() = None;
        *self.refresh_token.write() = None;
        self.bus.publish(CoreEvent::LoggedOut);
    }
}
```

### 1.2 lumen-core/src/api.rs — auto-refresh en 401

```rust
impl ApiClient {
    pub async fn request(&self, method: &str, path: &str,
                         body: Option<&serde_json::Value>) -> Result<Response, ApiError> {
        let res = self.raw_request(method, path, body).await?;
        if res.status() == 401 {
            // un solo retry con refresh
            if self.auth.try_refresh().await {
                return self.raw_request(method, path, body).await;
            }
            self.bus.publish(CoreEvent::LoggedOut); // session expirada -> login screen
        }
        Ok(res)
    }
}
```

Nota: ApiClient necesita un `Arc<AuthService>` (o un callback). Alternativa
mas desacoplada: el UiController intercepta el 401 — pero el retry debe
ser transparente en api.rs. Inyectar `auth: Arc<AuthService>` en el
constructor de ApiClient.

### 1.3 ctrl/auth.rs — logout completo

```rust
// UiController.logout() ahora:
async fn logout(&self) {
    self.presence.disconnect().await;          // Fase 3
    self.voice.leave_all().await;
    self.auth.logout().await;
    // push_shell resetea toda la UI
}
```

### 1.4 Persistencia del refresh token

Guardar en el mismo settings.json que ya usa el cliente
(`<config_dir>/lumen/settings.json`). El access token NO se persiste
(solo memoria). El refresh token si (30 dias).

```rust
// lumen-core/src/settings.rs
pub struct Settings {
    pub backend_url: String,
    pub refresh_token: Option<String>,  // nuevo
}
```

---

## Fase 2 — CRUD (cliente)

### 2.1 lumen-core/src/api.rs — nuevas llamadas

```rust
impl ApiClient {
    // servers
    pub async fn update_server(&self, id: &str, name: Option<&str>) -> Result<Server, ApiError> {
        self.patch(&format!("/api/servers/{id}"), &serde_json::json!({ "name": name })).await
    }
    pub async fn delete_server(&self, id: &str) -> Result<(), ApiError> {
        self.delete(&format!("/api/servers/{id}?confirm=true")).await
    }
    pub async fn leave_server(&self, id: &str) -> Result<(), ApiError> { ... }
    pub async fn regenerate_invite(&self, id: &str) -> Result<String, ApiError> { ... }
    pub async fn kick_member(&self, server_id: &str, user_id: &str) -> Result<(), ApiError> { ... }

    // channels
    pub async fn update_channel(&self, id: &str, patch: &serde_json::Value) -> Result<Channel, ApiError> { ... }
    pub async fn delete_channel(&self, id: &str) -> Result<(), ApiError> { ... }

    // messages
    pub async fn edit_message(&self, id: &str, content: &str) -> Result<TextMessage, ApiError> { ... }
    pub async fn delete_message(&self, id: &str) -> Result<(), ApiError> { ... }

    // profile
    pub async fn update_username(&self, username: &str) -> Result<User, ApiError> { ... }
    pub async fn change_password(&self, current: &str, new: &str) -> Result<(), ApiError> { ... }
    pub async fn delete_account(&self) -> Result<(), ApiError> { ... }

    // friends
    pub async fn remove_friend(&self, user_id: &str) -> Result<(), ApiError> { ... }

    // dms
    pub async fn delete_dm(&self, channel_id: &str) -> Result<(), ApiError> { ... }
}
```

### 2.2 EventBus — nuevos CoreEvent

```rust
pub enum CoreEvent {
    // ... existing ...
    ServerUpdated { server: Server },
    ServerDeleted { server_id: String },
    ServerLeft { server_id: String },
    ChannelUpdated { channel: Channel },
    ChannelDeleted { channel_id: String },
    MessageUpdated { message: TextMessage },      // edit
    MessageDeleted { channel_id: String, message_id: String },
    FriendsChanged,                                // remove friend -> reload
}
```

### 2.3 ctrl/shell.rs — context menus

Handler de acciones (Slint callback → controller):

```rust
// app.slint expone callbacks:
//   server-action(string action, int serverIndex)
//   channel-action(string action, int channelIndex)
//   message-action(string action, int messageIndex)

fn on_server_action(&self, action: &str, server_id: &str) {
    let shell = self.shell.clone();
    self.rt.spawn(async move {
        match action {
            "edit"      => { /* dialog nombre -> update_server */ }
            "invite"    => { /* regenerar + copiar al clipboard */ }
            "leave"     => { /* confirm dialog -> leave_server */ }
            "delete"    => { /* confirm dialog -> delete_server */ }
            _ => {}
        }
    });
}
```

### 2.4 model.rs — campos nuevos

```rust
// MessageItem (types.slint) gana:
//   edited: bool          -> mostrar "(editado)"
//   deleted: bool         -> mostrar placeholder "Mensaje eliminado"
//   reply_to: SharedString -> quote opcional
```

```rust
pub fn messages_model(msgs: &[TextMessage], self_user_id: &str) -> ModelRc<MessageItem> {
    msgs.iter().map(|m| MessageItem {
        // ... existing ...
        edited: m.edited_at.is_some(),
        deleted: m.deleted_at.is_some(),
    }).collect()
}
```

### 2.5 UI — .slint sketches

```slint
// ui/types.slint (extractos)
export struct MessageItem {
    // ... existing ...
    edited: bool,
    deleted: bool,
    reply_to: string,
}

// ui/components/message-row.slint (nuevo)
component MessageRow inherits Rectangle {
    in property <MessageItem> msg;
    // deleted -> placeholder opaco
    if !msg.deleted: Text { text: msg.content; }
    if msg.deleted: Text { text: "Mensaje eliminado"; color: gray; italic: true; }
    if msg.edited: Text { text: "(editado)"; font-size: 10px; color: gray; }
    // context menu via right-click callback -> message-action
}
```

---

## Fase 3 — Real-time / presencia (cliente) ← LA MAS GRANDE

### 3.1 lumen-core — nuevo modulo presence.rs

```rust
// crates/lumen-core/src/presence.rs
//! Presence WebSocket client (protocolo presence-v2).

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};

pub struct PresenceClient {
    url: String,
    token: RwLock<Option<String>>,
    bus: EventBus,
    tx: RwLock<Option<mpsc::UnboundedSender<PresenceOut>>>,
    task: RwLock<Option<JoinHandle<()>>>,
}

pub enum PresenceOut {
    Status(PresenceStatus),
    VoiceJoin { channel_id: String, server_id: String },
    VoiceLeave,
    Chat { channel_id: String, server_id: String, content: String, client_id: String },
    Typing { channel_id: String, server_id: String },
    Subscribe(String),
    DmSignal { to: String, kind: DmSignalKind, sdp: Option<String>, candidate: Option<Value> },
}

impl PresenceClient {
    pub async fn connect(&self, token: &str, username: &str, servers: &[String], friends: &[String]) { ... }
    pub async fn disconnect(&self) { ... }   // cierra WS + task
    pub async fn send(&self, msg: PresenceOut) { ... }

    // loop de lectura: parsea PresenceServerMessage -> CoreEvent
    async fn read_loop(&self, mut rx: SplitSink, mut stream: SplitStream) {
        while let Some(Ok(Message::Text(text))) = stream.next().await {
            let msg: PresenceServerMessage = serde_json::from_str(&text)?;
            match msg {
                PresenceServerMessage::Ready { online_friends, servers } =>
                    self.bus.publish(CoreEvent::PresenceReady { online_friends, servers }),
                PresenceServerMessage::FriendOnline { user_id, username } =>
                    self.bus.publish(CoreEvent::FriendOnline { user_id, username }),
                PresenceServerMessage::FriendOffline { user_id } =>
                    self.bus.publish(CoreEvent::FriendOffline { user_id }),
                PresenceServerMessage::FriendStatus { user_id, status } =>
                    self.bus.publish(CoreEvent::FriendStatus { user_id, status }),
                PresenceServerMessage::VoiceUpdate { server_id, channel_id, peers } =>
                    self.bus.publish(CoreEvent::VoiceOccupancyChanged { server_id, channel_id, peers }),
                PresenceServerMessage::MemberOnline { server_id, user_id, username } =>
                    self.bus.publish(CoreEvent::MemberOnline { server_id, user_id, username }),
                PresenceServerMessage::MemberOffline { server_id, user_id } =>
                    self.bus.publish(CoreEvent::MemberOffline { server_id, user_id }),
                PresenceServerMessage::Chat { channel_id, message } =>
                    self.bus.publish(CoreEvent::RealtimeMessage { channel_id, message }),
                PresenceServerMessage::ChatAck { client_id, message_id, created_at } =>
                    self.bus.publish(CoreEvent::ChatAck { client_id, message_id, created_at }),
                PresenceServerMessage::DmOffer { from, sdp } => { /* -> voice::join_dm */ }
                PresenceServerMessage::DmAnswer { from, sdp } => { /* -> voice */ }
                PresenceServerMessage::DmIce { from, candidate } => { /* -> voice */ }
                PresenceServerMessage::Error { code, .. } =>
                    self.bus.publish(CoreEvent::Error { message: code }),
                _ => {}
            }
        }
        // stream cerrado -> reconnect con backoff exponencial (1s, 2s, 4s, ... max 30s)
    }
}
```

### 3.2 EventBus — variantes nuevas

```rust
pub enum CoreEvent {
    // ... existing ...
    PresenceReady {
        online_friends: Vec<FriendPresence>,
        servers: Vec<ServerPresence>,
    },
    FriendOnline { user_id: String, username: String },
    FriendOffline { user_id: String },
    FriendStatus { user_id: String, status: PresenceStatus },
    VoiceOccupancyChanged {
        server_id: String, channel_id: String,
        peers: Vec<PeerLite>,
    },
    MemberOnline { server_id: String, user_id: String, username: String },
    MemberOffline { server_id: String, user_id: String },
    RealtimeMessage { channel_id: String, message: TextMessage },
    ChatAck { client_id: String, message_id: String, created_at: String },
    ChatError { client_id: String, code: String },
}
```

### 3.3 ShellState — estado nuevo

```rust
// state.rs
pub struct ShellState {
    // ... existing ...
    pub presence: PresenceState,          // nuevo
    pub pending_acks: RwLock<HashMap<String, PendingSend>>,  // clientId -> (channelId, content, sentAt)
    pub voice_occupancy: RwLock<HashMap<String, Vec<String>>>, // channelId -> [userId]
}

pub struct PresenceState {
    pub online_friends: RwLock<HashMap<String, FriendPresence>>,
    pub server_presence: RwLock<HashMap<String, ServerPresence>>,
}

pub struct FriendPresence {
    pub username: String,
    pub status: PresenceStatus,
}

pub struct ServerPresence {
    pub online_members: Vec<MemberLite>,
    pub voice_channels: Vec<VoiceChannelPresence>,
}

pub struct VoiceChannelPresence {
    pub channel_id: String,
    pub peers: Vec<MemberLite>,
}
```

### 3.4 Chat: envio con ACK + retransmision

```rust
// ctrl/chat.rs — on_send_message reescrito
fn on_send_message(&self, content: &str) {
    let client_id = uuid();  // por mensaje
    self.shell.pending_acks.write().insert(client_id.clone(), PendingSend {
        channel_id: self.shell.selected_channel_id.read().clone().unwrap(),
        server_id: ..., content: content.to_string(), sent_at: Instant::now(),
    });
    self.presence.send(PresenceOut::Chat { channel_id, server_id, content, client_id });
    // el mensaje aparece "pending" en la UI (opacity 0.5)
    // al recibir ChatAck -> marcar como enviado
}

// worker de retransmision (spawn en connect):
//  cada 3s, re-enviar pending_acks con sent_at > 3s (max 3 intentos)
//  al 4to intento -> CoreEvent::Error("no se pudo enviar") y quitar pending

// al recibir ChatAck:
fn on_chat_ack(&self, client_id: &str, message_id: &str) {
    let pending = self.shell.pending_acks.write().remove(client_id);
    if let Some(p) = pending {
        // mover al modelo de mensajes con message_id real
        self.push_messages();  // reload o insert local
    }
}
```

### 3.5 Paginacion: scroll up

```rust
// ctrl/chat.rs
fn on_scroll_to_top(&self) {
    let before = self.shell.oldest_message_at.read().clone();  // cursor
    let channel_id = self.shell.selected_channel_id.read().clone().unwrap();
    self.rt.spawn(async move {
        let msgs = self.api.get_messages_before(&channel_id, before.as_deref()).await?;
        // prepend a self.shell.messages + push
    });
}
```

### 3.6 Slint UI — presencia

```slint
// ui/components/member-list.slint (nuevo)
component MemberList inherits Rectangle {
    in property <[MemberItem]> members;
    // lista vertical: avatar + nombre + dot de estado
}

// ui/components/voice-channel.slint (modifica ChannelItem)
export struct ChannelItem {
    // ... existing ...
    peer_count: int,      // badge "3" cuando hay peers
    has_peers: bool,
}

// app.slint — callbacks nuevos:
//   presence-connected()
//   friend-online(string userId)
//   friend-offline(string userId)
//   voice-update(string channelId)
//   message-ack(string clientId, string messageId)
//   scroll-to-top()
```

### 3.7 lumen-voice — DataChannels

```rust
// client.rs — en VoiceSession::connect_peer, tras crear RTCPeerConnection:
let dc = pc.create_data_channel("chat", None).await?;
let mut dc_events = dc.receiver().events();
let tx = self.events_tx.clone();
tokio::spawn(async move {
    while let Some(ev) = dc_events.next().await {
        if let DataChannelEvent::Message(m) = ev {
            let _ = tx.send(VoiceEvent::DataChannelMessage(m.data.to_vec())).await;
        }
    }
});

// event.rs — nuevo:
pub enum VoiceEvent {
    // ... existing ...
    DataChannelMessage(Vec<u8>),   // de un peer en la llamada
}
```

Para DMs data-only (fuera de llamada de voz): `VoiceClient::join_dm(peer)`
establece peer connection SIN tracks de audio, solo data channel "dm".

### 3.8 voice.rs (Slint) — reenvio de DataChannel

```rust
// VoiceController: DataChannelMessage -> bus
//  - los peers de la llamada envian typing/chat -> CoreEvent::InCallChat { peer_id, bytes }
//  - el ChatController lo decodifica como JSON { type: "typing" | "chat", content }
```

---

## Fase 4 — OAuth / deep links (cliente)

### 4.1 main.rs — registro de protocolo

```rust
// Linux: el instalador (o setup manual) registra:
//   ~/.local/share/applications/lumen.desktop:
//     [Desktop Entry]
//     Exec=lumen %u
//     MimeType=x-scheme-handler/lumen;
//     NoDisplay=true

fn main() {
    // ... setup normal ...
    if let Some(link) = parse_deeplink(&std::env::args().collect::<Vec<_>>()) {
        match link {
            Deeplink::AuthCallback { token, refresh_token } => {
                // guardar en AuthService + persistir refresh token + arrancar sesion
            }
            Deeplink::Invite(code) => {
                // precargar el dialog de join con el codigo
            }
        }
    }
}

pub enum Deeplink {
    AuthCallback { token: String, refreshToken: Option<String> },
    Invite(String),
}
```

### 4.2 OAuth desde la UI

```rust
// ctrl/auth.rs — on_oauth_google
fn on_oauth(&self, provider: &str) {
    // abrir browser externo:
    //   xdg-open "https://api.dominio.com/api/oauth/{provider}?client=desktop"
    // (la app queda esperando el deeplink; si la app YA esta abierta,
    //  el OS la re-levanta con el argv -> parse_deeplink)
}
```

Nota: si la app ya esta corriendo, el deeplink llega como argv de una
segunda instancia. Manejar: single-instance lock (unix socket o flock) y
enviar el argv al proceso principal via IPC. Alternativa mas simple:
WebView embebido que intercepta la navegacion a `lumen://`. Documentar la
decision al implementar; empezar con browser externo + single-instance.

### 4.3 UI — login

```slint
// ui/views/login.slint — debajo del form clasico:
Row {
    Button { text: "Continuar con Google"; clicked => root.oauth-google(); }
    Button { text: "Continuar con GitHub"; clicked => root.oauth-github(); }
}
```

### 4.4 Avatares

```rust
// ctrl/settings.rs — cambio de avatar
async fn upload_avatar(&self, path: &Path) {
    let bytes = std::fs::read(path)?;
    if bytes.len() > 5 * 1024 * 1024 { /* error */ }
    self.api.put_bytes("/api/me/avatar", bytes, "image/png").await?;
    // reload /api/me -> push user
}

// model.rs — AvatarItem/UserItem gana avatar_url: string
// la UI renderiza la imagen si avatar_url != "" (fallback: silueta skin)
```

---

## Fase 5 — Moderacion (cliente)

### 5.1 API

```rust
pub async fn ban_member(&self, server_id: &str, user_id: &str, reason: Option<&str>) -> Result<(), ApiError> { ... }
pub async fn unban_member(&self, server_id: &str, user_id: &str) -> Result<(), ApiError> { ... }
pub async fn list_bans(&self, server_id: &str) -> Result<Vec<Ban>, ApiError> { ... }
pub async fn block_user(&self, user_id: &str) -> Result<(), ApiError> { ... }
pub async fn unblock_user(&self, user_id: &str) -> Result<(), ApiError> { ... }
pub async fn report(&self, target_type: &str, target_id: &str, reason: Option<&str>) -> Result<(), ApiError> { ... }
pub async fn transfer_server(&self, server_id: &str, user_id: &str) -> Result<(), ApiError> { ... }
```

### 5.2 UI

```slint
// ui/views/server-settings.slint (nuevo)
//   tabs: General | Members | Bans
//   Members: lista con boton Kick / Ban (owner only, no owner mismo)
//   Bans: lista con boton Unban

// context menu usuario (friends list / member list):
//   Block / Report
// context menu mensaje:
//   Report

// transfer: server settings -> dropdown de miembros -> Transfer
```

### 5.3 Eventos

```rust
// kick/ban remoto: el PresenceHubDO cierra el acceso; el cliente recibe
// MemberOffline o error. Estado local: si el server deja de incluirte,
// reload servers (CoreEvent::ServersLoaded) al recibir error kicked.
```

---

## Fase 6 — Polish (cliente)

| Item | Cambio cliente |
|---|---|
| 6.1 Replies | MessageItem.reply_to + UI quote; composer envia replyTo |
| 6.2 Attachments | file picker -> PUT /api/uploads -> R2 url en mensaje; MessageItem.attachment_url + render imagen |
| 6.3 Reactions | mensaje hover -> barra emoji; GET/PUT reaction; CoreEvent::ReactionChanged |
| 6.4 Pins | context menu pin; vista de pins |
| 6.5 Search | barra de busqueda -> GET /api/search -> resultados |
| 6.6 Roles | server settings -> roles editor (bitmask checkboxes) |
| 6.7 Categories | grouping visual de channels por category |
| 6.8 User search | dialog "agregar amigo" con autocomplete |
| 6.9 Admin | panel si ADMIN_IDS contiene al user |

---

## Orden de integracion recomendado (por fase, dentro del cliente)

1. **Fase 1**: auth.rs refresh + api.rs retry + settings.rs persistencia
   (1-2 dias)
2. **Fase 2**: api.rs calls + CoreEvent nuevos + context menus + model.rs
   campos (2-3 dias)
3. **Fase 3**: presence.rs module + EventBus variantes + ShellState +
   chat ACK/retransmision + DataChannels + UI presencia (4-6 dias)
4. **Fase 4**: main.rs deeplink + single-instance + login OAuth + avatares
   (2 dias)
5. **Fase 5**: api calls + server settings UI + context menus (1-2 dias)
6. **Fase 6**: incremental

## Testing del cliente

| Nivel | Que | Como |
|---|---|---|
| Unit (lumen-core) | parse de PresenceServerMessage, dedup ACK, retransmision, is_online | `cargo test -p lumen-core` |
| Unit (lumen-voice) | data channel message routing | `cargo test -p lumen-voice` |
| Integration (backend) | smoke-ws.mjs actualizado con presencia + chat RT + flush | `pnpm smoke` |
| E2E manual | 2 clientes: presencia, voice occupancy, chat RT, DM P2P | `scripts/lumen-dual.sh` (2 instancias locales) |
| E2E con backend real | deploy preview + 2 clientes contra workers.dev | Backend_URL env |

## Metricas de rendimiento del cliente (a medir)

- Latencia message -> ACK (objetivo < 150ms local, < 400ms WAN)
- RAM con 200 friends + 10 servers (objetivo < 50 MB extra)
- Reconnect time tras caida de red (objetivo < 5s con backoff)
- CPU en idle con presence WS (objetivo ~0, solo read_loop bloqueado en await)
