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
}

/** Messages the client sends to the channel Durable Object. */
export type ClientMessage =
  | { type: "join"; channelId: string; userId: string }
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
}

export interface Channel {
  id: string;
  /** Empty for DM channels. */
  serverId: string;
  name: string;
  kind: "text" | "voice" | "dm";
  createdAt: string;
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
}

export interface AuthResponse {
  token: string;
  user: User;
}

/** Shape compatible with RTCIceServer (client casts). */
export interface IceServerConfig {
  urls: string | string[];
  username?: string;
  credential?: string;
}

export interface RealtimeConfig {
  iceServers: IceServerConfig[];
}
