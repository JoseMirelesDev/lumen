/**
 * Lumen signaling protocol + REST API types — v1.
 *
 * Single source of truth shared by backend (Worker/DO) and desktop client.
 * Full spec with rationale: docs/protocol.md
 */

// ---------------------------------------------------------------------------
// WebSocket signaling (client <-> LumenChannelDO)
// ---------------------------------------------------------------------------

export type PresenceStatus = "online" | "idle" | "offline";

export interface PeerInfo {
  peerId: string;
  userId: string;
  username: string;
}

/** Messages the client sends to the channel Durable Object. */
export type ClientMessage =
  | { type: "join"; channelId: string; userId: string; username: string }
  | { type: "offer"; to: string; sdp: string }
  | { type: "answer"; to: string; sdp: string }
  | { type: "ice-candidate"; to: string; candidate: unknown }
  | { type: "presence"; status: PresenceStatus }
  | { type: "ping" };

/** Messages the channel Durable Object sends to clients. */
export type ServerMessage =
  | { type: "joined"; peerId: string; peers: PeerInfo[] }
  | { type: "peer-joined"; peer: PeerInfo }
  | { type: "peer-left"; peerId: string }
  | { type: "offer"; from: string; sdp: string }
  | { type: "answer"; from: string; sdp: string }
  | { type: "ice-candidate"; from: string; candidate: unknown }
  | { type: "presence"; userId: string; status: PresenceStatus }
  | { type: "pong" }
  | { type: "error"; code: string; message: string };

// ---------------------------------------------------------------------------
// REST API
// ---------------------------------------------------------------------------

export interface User {
  id: string;
  username: string;
  lastSeen: string;
  createdAt: string;
}

export interface Server {
  id: string;
  name: string;
  ownerId: string;
  inviteCode: string;
  createdAt: string;
  /** R2 key (migration 0003/0004). */
  icon?: string | null;
}

export interface Channel {
  id: string;
  /** Empty for DM channels. */
  serverId: string;
  name: string;
  kind: "text" | "voice" | "dm";
  createdAt: string;
  /** Migration 0003. */
  topic?: string | null;
  /** Manual ordering within the server (migration 0003). */
  position?: number;
}

export interface ServerWithChannels {
  server: Server;
  channels: Channel[];
}

export interface FriendshipRequest {
  id: string;
  user: User;
  /** "incoming" = someone asked me; "outgoing" = I asked them */
  direction: "incoming" | "outgoing";
  createdAt: string;
}

export interface FriendInfo {
  user: User;
  /** shared servers count, for display */
  sharedServers: number;
}

export interface DmSummary {
  channel: Channel;
  /** the other participant's username */
  otherUsername: string;
}

export interface TextMessage {
  id: string;
  channelId: string;
  authorId: string;
  authorName: string;
  content: string;
  createdAt: string;
  /** Set when edited (migration 0003). */
  editedAt?: string | null;
  /** Set when soft-deleted — clients render a placeholder (migration 0003). */
  deletedAt?: string | null;
  /** Id of the replied-to message (migration 0003, Fase 6.2). */
  replyTo?: string | null;
  /** R2 URL for attachments (Fase 6.4). */
  attachmentUrl?: string | null;
}

export interface AuthResponse {
  token: string;
  refreshToken: string;
  user: User;
}

/** POST /api/auth/refresh — rotation response (ADR-0007). */
export interface RefreshResponse {
  token: string;
  refreshToken: string;
}

/** Message block row (ADR-0004): 1 row = up to 50 packed messages. */
export interface MessageBlock {
  id: string;
  channelId: string;
  count: number;
  firstAt: string;
  lastAt: string;
}

/** PATCH /api/messages/:id response (Fase 2, provisional per ADR-0010). */
export interface EditMessageResult {
  id: string;
  content: string;
  editedAt: string;
}

/** Minimal permission model (Fase 2/5): owner vs member. */
export type ServerRole = "owner" | "member";

/** Shape compatible with RTCIceServer (client casts). */
export interface IceServerConfig {
  urls: string | string[];
  username?: string;
  credential?: string;
}

export interface RealtimeConfig {
  iceServers: IceServerConfig[];
}

// ---------------------------------------------------------------------------
// Presence v2 (WS /api/presence → PresenceHubDO) — protocol/presence-v2.md.
// The v1 PresenceStatus above ("online"|"idle"|"offline") stays for the
// ChannelDO voice broadcasts; the presence hub uses its own status set.
// ---------------------------------------------------------------------------

export type PresenceV2Status = "online" | "idle" | "dnd";

export interface PeerLite {
  userId: string;
  username: string;
}

export interface OnlineFriendLite {
  userId: string;
  username: string;
  status: PresenceV2Status;
}

export interface VoiceChannelPresence {
  channelId: string;
  peers: PeerLite[];
}

export interface ServerPresence {
  serverId: string;
  onlineMembers: PeerLite[];
  voiceChannels: VoiceChannelPresence[];
}

/** One buffered chat message (ADR-0004): packed N-per-row in message_blocks. */
export interface BufferedMessage {
  id: string;
  authorId: string;
  authorName: string;
  content: string;
  createdAt: string;
  /** Set when edited (ADR-0010, chat-edit). */
  editedAt?: string | null;
  /** Set when soft-deleted (ADR-0010, chat-delete). */
  deletedAt?: string | null;
  /** Id of the replied-to message (Fase 6.2). */
  replyTo?: string | null;
  /** R2 URL for attachments (Fase 6.4). */
  attachmentUrl?: string | null;
}

export type DmSignalKind = "offer" | "answer" | "ice";

/** Messages the client sends to the PresenceHubDO. */
export type PresenceClientMessage =
  | { type: "ready" }
  | { type: "status"; status: PresenceV2Status }
  | { type: "voice-join"; channelId: string; serverId: string }
  | { type: "voice-leave" }
  | { type: "chat"; channelId: string; serverId: string; content: string; clientId: string; replyTo?: string; attachmentUrl?: string }
  | { type: "chat-edit"; channelId: string; serverId: string; messageId: string; content: string; clientId: string }
  | { type: "chat-delete"; channelId: string; serverId: string; messageId: string; clientId: string }
  | { type: "typing"; channelId: string; serverId: string }
  | { type: "subscribe"; channelId: string }
  | { type: "unsubscribe"; channelId: string }
  | { type: "dm-signal"; to: string; kind: DmSignalKind; sdp?: string; candidate?: unknown }
  | { type: "reaction-toggle"; channelId: string; serverId: string; messageId: string; emoji: string }
  | { type: "ping" };

/** Messages the PresenceHubDO sends to clients. */
export type PresenceServerMessage =
  | { type: "ready"; onlineFriends: OnlineFriendLite[]; servers: ServerPresence[] }
  | { type: "friend-online"; userId: string; username: string }
  | { type: "friend-offline"; userId: string }
  | { type: "friend-status"; userId: string; status: PresenceV2Status }
  | { type: "voice-update"; serverId: string; channelId: string; peers: PeerLite[] }
  | { type: "member-online"; serverId: string; userId: string; username: string }
  | { type: "member-offline"; serverId: string; userId: string }
  | { type: "typing"; channelId: string; userId: string }
  | { type: "subscribe-ack"; channelId: string }
  | { type: "chat"; channelId: string; message: BufferedMessage }
  | { type: "chat-ack"; clientId: string; messageId: string; createdAt: string }
  | { type: "chat-edit-ack"; clientId: string; messageId: string }
  | { type: "chat-delete-ack"; clientId: string; messageId: string }
  | { type: "chat-edited"; channelId: string; message: { id: string; content: string; editedAt: string } }
  | { type: "chat-deleted"; channelId: string; messageId: string }
  | { type: "chat-error"; clientId: string; code: string }
  | { type: "reaction"; channelId: string; messageId: string; emoji: string; userId: string; added: boolean }
  | { type: "dm-offer"; from: string; sdp: string }
  | { type: "dm-answer"; from: string; sdp: string }
  | { type: "dm-ice"; from: string; candidate: unknown }
  | { type: "pong" }
  | { type: "error"; code: string; message: string };
