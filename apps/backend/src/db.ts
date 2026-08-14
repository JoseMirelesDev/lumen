import type {
  Channel,
  FriendInfo,
  FriendshipRequest,
  Server,
  ServerWithChannels,
  TextMessage,
  User,
} from "@lumen/protocol";

/**
 * Typed D1 access layer. Row types mirror the snake_case schema in
 * migrations/0001_init.sql; protocol types are produced by the rowTo* helpers.
 */

export interface UserRow {
  id: string;
  username: string;
  password_salt: string;
  password_hash: string;
  last_seen: string;
  created_at: string;
  /** R2 key (migration 0003). */
  avatar?: string | null;
  password_version?: number;
  /** Soft delete (migration 0003): login/access are rejected when set. */
  deleted_at?: string | null;
  /** Fase 4 (migration 0005). */
  email?: string | null;
  oauth_provider?: string | null;
  oauth_id?: string | null;
}

export interface ServerRow {
  id: string;
  name: string;
  owner_id: string;
  invite_code: string;
  created_at: string;
  /** R2 key (migration 0003). */
  icon?: string | null;
  invite_regenerated_at?: string | null;
}

export interface ChannelRow {
  id: string;
  server_id: string | null;
  name: string;
  kind: "text" | "voice" | "dm";
  created_at: string;
  /** Migration 0003. */
  topic?: string | null;
  position?: number;
}

export interface FriendshipRow {
  id: string;
  user_id: string;
  friend_id: string;
  status: "pending" | "accepted";
  requested_by: string;
  created_at: string;
}

interface FriendRequestRow {
  id: string;
  created_at: string;
  uid: string;
  username: string;
  last_seen: string;
  ucreated: string;
}

interface SharedCountRow {
  uid: string;
  n: number;
}

export function rowToUser(r: UserRow): User {
  return { id: r.id, username: r.username, lastSeen: r.last_seen, createdAt: r.created_at };
}

export function rowToServer(r: ServerRow): Server {
  return {
    id: r.id,
    name: r.name,
    ownerId: r.owner_id,
    inviteCode: r.invite_code,
    createdAt: r.created_at,
    ...(r.icon ? { icon: r.icon } : {}),
  };
}

export function rowToChannel(r: ChannelRow): Channel {
  return {
    id: r.id,
    serverId: r.server_id ?? "",
    name: r.name,
    kind: r.kind,
    createdAt: r.created_at,
    ...(r.topic !== undefined && r.topic !== null ? { topic: r.topic } : {}),
    ...(r.position !== undefined ? { position: r.position } : {}),
  };
}

// ---------------------------------------------------------------------------
// users
// ---------------------------------------------------------------------------

export async function getUserById(db: D1Database, id: string): Promise<UserRow | null> {
  return (await db.prepare("SELECT * FROM users WHERE id = ?").bind(id).first()) as UserRow | null;
}

export async function getUserByUsername(db: D1Database, username: string): Promise<UserRow | null> {
  // username column is COLLATE NOCASE in the schema.
  return (await db.prepare("SELECT * FROM users WHERE username = ?").bind(username).first()) as
    | UserRow
    | null;
}

export async function createUser(
  db: D1Database,
  args: { id: string; username: string; salt: string; hash: string },
): Promise<void> {
  await db
    .prepare("INSERT INTO users (id, username, password_salt, password_hash) VALUES (?, ?, ?, ?)")
    .bind(args.id, args.username, args.salt, args.hash)
    .run();
}

export async function updateLastSeen(db: D1Database, userId: string): Promise<void> {
  await db
    .prepare("UPDATE users SET last_seen = ? WHERE id = ?")
    .bind(new Date().toISOString(), userId)
    .run();
}

/** Rename a user; the UNIQUE NOCASE column raises on conflict (caller maps to 409). */
export async function updateUsername(db: D1Database, userId: string, username: string): Promise<void> {
  await db.prepare("UPDATE users SET username = ? WHERE id = ?").bind(username, userId).run();
}

export async function updatePassword(
  db: D1Database,
  userId: string,
  salt: string,
  hash: string,
): Promise<void> {
  await db
    .prepare("UPDATE users SET password_salt = ?, password_hash = ? WHERE id = ?")
    .bind(salt, hash, userId)
    .run();
}

/** Soft delete: login/access rejected from now on; sessions revoked by caller. */
export async function softDeleteUser(db: D1Database, userId: string): Promise<void> {
  await db
    .prepare("UPDATE users SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL")
    .bind(new Date().toISOString(), userId)
    .run();
}

export async function updateAvatar(db: D1Database, userId: string, key: string): Promise<void> {
  await db.prepare("UPDATE users SET avatar = ? WHERE id = ?").bind(key, userId).run();
}

// ---------------------------------------------------------------------------
// servers + channels
// ---------------------------------------------------------------------------

export async function createServerWithDefaults(
  db: D1Database,
  args: { id: string; name: string; ownerId: string; inviteCode: string },
): Promise<{ server: Server; channels: Channel[] }> {
  const generalId = crypto.randomUUID();
  const voiceId = crypto.randomUUID();
  const now = new Date().toISOString();
  await db.batch([
    db
      .prepare("INSERT INTO servers (id, name, owner_id, invite_code) VALUES (?, ?, ?, ?)")
      .bind(args.id, args.name, args.ownerId, args.inviteCode),
    db
      .prepare("INSERT INTO channels (id, server_id, name, kind) VALUES (?, ?, 'general', 'text')")
      .bind(generalId, args.id),
    db
      .prepare("INSERT INTO channels (id, server_id, name, kind) VALUES (?, ?, 'General', 'voice')")
      .bind(voiceId, args.id),
    db.prepare("INSERT INTO server_members (server_id, user_id) VALUES (?, ?)").bind(args.id, args.ownerId),
  ]);
  return {
    server: {
      id: args.id,
      name: args.name,
      ownerId: args.ownerId,
      inviteCode: args.inviteCode,
      createdAt: now,
    },
    channels: [
      { id: generalId, serverId: args.id, name: "general", kind: "text", createdAt: now },
      { id: voiceId, serverId: args.id, name: "General", kind: "voice", createdAt: now },
    ],
  };
}

export async function getServer(db: D1Database, id: string): Promise<ServerRow | null> {
  return (await db.prepare("SELECT * FROM servers WHERE id = ?").bind(id).first()) as ServerRow | null;
}

export async function getServerByInviteCode(db: D1Database, inviteCode: string): Promise<ServerRow | null> {
  return (await db.prepare("SELECT * FROM servers WHERE invite_code = ?").bind(inviteCode).first()) as
    | ServerRow
    | null;
}

export async function addMember(db: D1Database, serverId: string, userId: string): Promise<void> {
  await db
    .prepare("INSERT OR IGNORE INTO server_members (server_id, user_id) VALUES (?, ?)")
    .bind(serverId, userId)
    .run();
}

export async function isMember(db: D1Database, serverId: string, userId: string): Promise<boolean> {
  const row = await db
    .prepare("SELECT 1 FROM server_members WHERE server_id = ? AND user_id = ?")
    .bind(serverId, userId)
    .first();
  return row !== null;
}

export async function getChannelsForServer(db: D1Database, serverId: string): Promise<Channel[]> {
  const rows = await db
    .prepare("SELECT * FROM channels WHERE server_id = ? ORDER BY created_at ASC")
    .bind(serverId)
    .all<ChannelRow>();
  return rows.results.map(rowToChannel);
}

/** Member id + username, for presence/voice display. */
export async function getMembers(db: D1Database, serverId: string): Promise<{ id: string; username: string }[]> {
  const rows = await db
    .prepare(
      "SELECT u.id, u.username FROM server_members m JOIN users u ON u.id = m.user_id WHERE m.server_id = ?",
    )
    .bind(serverId)
    .all<{ id: string; username: string }>();
  return rows.results;
}

export async function listServersForUser(db: D1Database, userId: string): Promise<ServerWithChannels[]> {
  const { results: serverResults } = await db
    .prepare(
      "SELECT s.* FROM servers s JOIN server_members m ON m.server_id = s.id WHERE m.user_id = ? ORDER BY s.created_at ASC",
    )
    .bind(userId)
    .all();
  const servers = serverResults as unknown as ServerRow[];
  if (servers.length === 0) return [];

  const placeholders = servers.map(() => "?").join(",");
  const { results: channelResults } = await db
    .prepare(
      `SELECT * FROM channels WHERE server_id IN (${placeholders}) ORDER BY created_at ASC`,
    )
    .bind(...servers.map((s) => s.id))
    .all();
  const channelsByServer = new Map<string, Channel[]>();
  for (const row of channelResults as unknown as ChannelRow[]) {
    const serverId = row.server_id ?? "";
    const list = channelsByServer.get(serverId) ?? [];
    list.push(rowToChannel(row));
    channelsByServer.set(serverId, list);
  }
  return servers.map((s) => ({ server: rowToServer(s), channels: channelsByServer.get(s.id) ?? [] }));
}

export async function createChannel(
  db: D1Database,
  args: { id: string; serverId: string; name: string; kind: "text" | "voice" },
): Promise<Channel> {
  await db
    .prepare("INSERT INTO channels (id, server_id, name, kind) VALUES (?, ?, ?, ?)")
    .bind(args.id, args.serverId, args.name, args.kind)
    .run();
  return {
    id: args.id,
    serverId: args.serverId,
    name: args.name,
    kind: args.kind,
    createdAt: new Date().toISOString(),
  };
}

export async function getChannel(db: D1Database, id: string): Promise<ChannelRow | null> {
  return (await db.prepare("SELECT * FROM channels WHERE id = ?").bind(id).first()) as ChannelRow | null;
}

export async function isOwner(db: D1Database, serverId: string, userId: string): Promise<boolean> {
  const row = await db
    .prepare("SELECT 1 FROM servers WHERE id = ? AND owner_id = ?")
    .bind(serverId, userId)
    .first();
  return row !== null;
}

export async function updateServer(
  db: D1Database,
  id: string,
  patch: { name?: string; icon?: string },
): Promise<ServerRow | null> {
  const sets: string[] = [];
  const params: string[] = [];
  if (patch.name !== undefined) {
    sets.push("name = ?");
    params.push(patch.name);
  }
  if (patch.icon !== undefined) {
    sets.push("icon = ?");
    params.push(patch.icon);
  }
  if (sets.length === 0) return getServer(db, id);
  await db.prepare(`UPDATE servers SET ${sets.join(", ")} WHERE id = ?`).bind(...params, id).run();
  return getServer(db, id);
}

/** Hard cascade: server_members/channels/messages/message_blocks/bans have ON DELETE CASCADE. */
export async function deleteServer(db: D1Database, id: string): Promise<void> {
  await db.prepare("DELETE FROM servers WHERE id = ?").bind(id).run();
}

export async function removeMember(db: D1Database, serverId: string, userId: string): Promise<void> {
  await db
    .prepare("DELETE FROM server_members WHERE server_id = ? AND user_id = ?")
    .bind(serverId, userId)
    .run();
}

export async function regenerateInvite(db: D1Database, serverId: string, code: string): Promise<void> {
  await db
    .prepare("UPDATE servers SET invite_code = ?, invite_regenerated_at = ? WHERE id = ?")
    .bind(code, new Date().toISOString(), serverId)
    .run();
}

export async function updateChannel(
  db: D1Database,
  id: string,
  patch: { name?: string; topic?: string | null; position?: number },
): Promise<ChannelRow | null> {
  const sets: string[] = [];
  const params: unknown[] = [];
  if (patch.name !== undefined) {
    sets.push("name = ?");
    params.push(patch.name);
  }
  if (patch.topic !== undefined) {
    sets.push("topic = ?");
    params.push(patch.topic);
  }
  if (patch.position !== undefined) {
    sets.push("position = ?");
    params.push(patch.position);
  }
  if (sets.length === 0) return getChannel(db, id);
  await db.prepare(`UPDATE channels SET ${sets.join(", ")} WHERE id = ?`).bind(...params, id).run();
  return getChannel(db, id);
}

/** Channel delete cascades messages + message_blocks (FK ON DELETE CASCADE). */
export async function deleteChannel(db: D1Database, id: string): Promise<void> {
  await db.prepare("DELETE FROM channels WHERE id = ?").bind(id).run();
}

// ---------------------------------------------------------------------------
// DMs (1:1 channels of kind 'dm')
// ---------------------------------------------------------------------------

/** Find the existing 1:1 DM channel between two users, if any (either member may have soft-deleted). */
export async function getDmChannelBetween(
  db: D1Database,
  userA: string,
  userB: string,
): Promise<ChannelRow | null> {
  const rows = await db
    .prepare(
      `SELECT c.* FROM channels c
       JOIN dm_members a ON a.channel_id = c.id AND a.user_id = ?
       JOIN dm_members b ON b.channel_id = c.id AND b.user_id = ?
       WHERE c.kind = 'dm' LIMIT 1`,
    )
    .bind(userA, userB)
    .all<ChannelRow>();
  return rows.results[0] ?? null;
}

export async function createDmChannel(db: D1Database, id: string, userA: string, userB: string): Promise<ChannelRow> {
  await db
    .prepare("INSERT INTO channels (id, server_id, name, kind) VALUES (?, NULL, ?, 'dm')")
    .bind(id, "dm")
    .run();
  await db
    .prepare("INSERT INTO dm_members (channel_id, user_id) VALUES (?, ?), (?, ?)")
    .bind(id, userA, id, userB)
    .run();
  return { id, server_id: null, name: "dm", kind: "dm", created_at: new Date().toISOString() };
}

// ---------------------------------------------------------------------------
// Fase 3 — presence context + message blocks
// ---------------------------------------------------------------------------

/** Server ids where the user is a member + accepted-friend ids (one query each,
 *  used by the /api/presence upgrade). */
export async function getUserPresenceContext(
  db: D1Database,
  userId: string,
): Promise<{ servers: string[]; friends: string[] }> {
  const [servers, friends] = await db.batch([
    db
      .prepare("SELECT server_id FROM server_members WHERE user_id = ?")
      .bind(userId),
    db
      .prepare(
        `SELECT friend_id FROM friendships WHERE user_id = ? AND status = 'accepted'
         UNION
         SELECT user_id FROM friendships WHERE friend_id = ? AND status = 'accepted'`,
      )
      .bind(userId, userId),
  ]);
  const serverIds = (servers!.results as { server_id: string }[]).map((r) => r.server_id);
  const friendIds = (friends!.results as { friend_id: string }[]).map((r) => r.friend_id);
  return { servers: serverIds, friends: friendIds };
}

/** One packed block (ADR-0004), newest first, cursor = (lastAt, id) tuple
 *  (P5: composite cursor — last_at alone can collide at ms resolution). */
export async function getMessageBlocks(
  db: D1Database,
  channelId: string,
  beforeAt?: string,
  beforeId?: string,
): Promise<{ id: string; channelId: string; count: number; firstAt: string; lastAt: string; messages: string }[]> {
  const rows = await db
    .prepare(
      `SELECT id, channel_id, count, first_at, last_at, messages
       FROM message_blocks
       WHERE channel_id = ? ${beforeAt && beforeId ? "AND (last_at, id) < (?, ?)" : ""}
       ORDER BY last_at DESC, id DESC
       LIMIT 1`,
    )
    .bind(channelId, ...(beforeAt && beforeId ? [beforeAt, beforeId] : []))
    .all<{
      id: string;
      channel_id: string;
      count: number;
      first_at: string;
      last_at: string;
      messages: string;
    }>();
  return rows.results.map((r) => ({
    id: r.id,
    channelId: r.channel_id,
    count: r.count,
    firstAt: r.first_at,
    lastAt: r.last_at,
    messages: r.messages,
  }));
}

/**
 * Unpack a block's JSON into protocol TextMessage shape (deleted entries keep
 * the row with deletedAt so clients render the placeholder, ADR-0010).
 */
export function blockMessagesToText(messagesJson: string, channelId: string): TextMessage[] {
  const entries = JSON.parse(messagesJson) as Array<{
    id: string;
    authorId: string;
    authorName: string;
    content: string;
    createdAt: string;
    editedAt?: string | null;
    deletedAt?: string | null;
    replyTo?: string | null;
    attachmentUrl?: string | null;
  }>;
  return entries.map((m) => ({
    id: m.id,
    channelId,
    authorId: m.authorId,
    authorName: m.authorName,
    content: m.content,
    createdAt: m.createdAt,
    ...(m.editedAt ? { editedAt: m.editedAt } : {}),
    ...(m.deletedAt ? { deletedAt: m.deletedAt } : {}),
    ...(m.replyTo ? { replyTo: m.replyTo } : {}),
    ...(m.attachmentUrl ? { attachmentUrl: m.attachmentUrl } : {}),
  }));
}

/** DM channels of a user, newest first, with the other participant's name.
 *  Only channels where MY membership is active appear; the other side's row
 *  may be soft-deleted (their view) without hiding the channel from me. */
export async function listDmChannelsForUser(  db: D1Database,
  userId: string,
): Promise<{ channel: ChannelRow; otherUsername: string }[]> {
  const rows = await db
    .prepare(
      `SELECT c.*, u.username AS other_username FROM channels c
       JOIN dm_members m ON m.channel_id = c.id AND m.user_id = ? AND m.deleted_at IS NULL
       JOIN dm_members o ON o.channel_id = c.id AND o.user_id != ?
       JOIN users u ON u.id = o.user_id
       WHERE c.kind = 'dm' ORDER BY c.created_at DESC`,
    )
    .bind(userId, userId)
    .all<ChannelRow & { other_username: string }>();
  return rows.results.map((r) => ({ channel: r, otherUsername: r.other_username }));
}

export async function isDmMember(db: D1Database, channelId: string, userId: string): Promise<boolean> {
  const row = await db
    .prepare(
      "SELECT 1 FROM dm_members WHERE channel_id = ? AND user_id = ? AND deleted_at IS NULL",
    )
    .bind(channelId, userId)
    .first();
  return row !== null;
}

/** Soft delete per user: mark MY membership deleted; the other side keeps the channel. */
export async function removeDmMember(db: D1Database, channelId: string, userId: string): Promise<void> {
  await db
    .prepare(
      "UPDATE dm_members SET deleted_at = ? WHERE channel_id = ? AND user_id = ? AND deleted_at IS NULL",
    )
    .bind(new Date().toISOString(), channelId, userId)
    .run();
}

/** Re-open a soft-deleted DM: restore MY membership row. */
export async function restoreDmMember(db: D1Database, channelId: string, userId: string): Promise<void> {
  await db
    .prepare("UPDATE dm_members SET deleted_at = NULL WHERE channel_id = ? AND user_id = ?")
    .bind(channelId, userId)
    .run();
}

// ---------------------------------------------------------------------------
// messages
// ---------------------------------------------------------------------------

export async function insertMessage(
  db: D1Database,
  args: { id: string; channelId: string; authorId: string; content: string },
): Promise<void> {
  await db
    .prepare("INSERT INTO messages (id, channel_id, author_id, content) VALUES (?, ?, ?, ?)")
    .bind(args.id, args.channelId, args.authorId, args.content)
    .run();
}

export async function listMessages(db: D1Database, channelId: string, limit: number): Promise<TextMessage[]> {
  const { results } = await db
    .prepare(
      `SELECT m.id, m.channel_id, m.author_id, m.content, m.created_at, m.edited_at, m.deleted_at, m.reply_to, u.username
       FROM messages m JOIN users u ON u.id = m.author_id
       WHERE m.channel_id = ? ORDER BY m.created_at ASC, m.id ASC LIMIT ?`,
    )
    .bind(channelId, limit)
    .all();
  return (results as unknown as Array<{
    id: string;
    channel_id: string;
    author_id: string;
    content: string;
    created_at: string;
    edited_at: string | null;
    deleted_at: string | null;
    reply_to: string | null;
    username: string;
  }>).map((r) => ({
    id: r.id,
    channelId: r.channel_id,
    authorId: r.author_id,
    authorName: r.username,
    content: r.content,
    createdAt: r.created_at,
    ...(r.edited_at ? { editedAt: r.edited_at } : {}),
    ...(r.deleted_at ? { deletedAt: r.deleted_at } : {}),
    ...(r.reply_to ? { replyTo: r.reply_to } : {}),
  }));
}

/** Message with the author's username (for edit responses). */
export async function getMessage(db: D1Database, id: string): Promise<TextMessage | null> {
  const row = await db
    .prepare(
      `SELECT m.id, m.channel_id, m.author_id, m.content, m.created_at, m.edited_at, m.deleted_at, m.reply_to, u.username
       FROM messages m JOIN users u ON u.id = m.author_id WHERE m.id = ?`,
    )
    .bind(id)
    .first();
  if (!row) return null;
  const r = row as {
    id: string;
    channel_id: string;
    author_id: string;
    content: string;
    created_at: string;
    edited_at: string | null;
    deleted_at: string | null;
    reply_to: string | null;
    username: string;
  };
  return {
    id: r.id,
    channelId: r.channel_id,
    authorId: r.author_id,
    authorName: r.username,
    content: r.content,
    createdAt: r.created_at,
    ...(r.edited_at ? { editedAt: r.edited_at } : {}),
    ...(r.deleted_at ? { deletedAt: r.deleted_at } : {}),
    ...(r.reply_to ? { replyTo: r.reply_to } : {}),
  };
}

/** Edit (only while not deleted); sets edited_at. Returns null if not found/deleted. */
export async function updateMessageContent(
  db: D1Database,
  id: string,
  content: string,
): Promise<TextMessage | null> {
  const row = await db
    .prepare(
      `UPDATE messages SET content = ?, edited_at = ? WHERE id = ? AND deleted_at IS NULL`,
    )
    .bind(content, new Date().toISOString(), id)
    .run();
  if (row.meta.changes === 0) return null;
  return getMessage(db, id);
}

/** Soft delete: the row stays (placeholder on read); FK keeps reply targets. */
export async function softDeleteMessage(db: D1Database, id: string): Promise<void> {
  await db
    .prepare("UPDATE messages SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL")
    .bind(new Date().toISOString(), id)
    .run();
}

// ---------------------------------------------------------------------------
// friendships
// ---------------------------------------------------------------------------

export async function listFriends(db: D1Database, userId: string): Promise<FriendInfo[]> {
  const { results } = await db
    .prepare(
      `SELECT u.id, u.username, u.last_seen, u.created_at
       FROM friendships f JOIN users u ON u.id = f.friend_id
       WHERE f.user_id = ? AND f.status = 'accepted'
       UNION
       SELECT u.id, u.username, u.last_seen, u.created_at
       FROM friendships f JOIN users u ON u.id = f.user_id
       WHERE f.friend_id = ? AND f.status = 'accepted'`,
    )
    .bind(userId, userId)
    .all();
  const friends = results as unknown as UserRow[];
  if (friends.length === 0) return [];

  const placeholders = friends.map(() => "?").join(",");
  const { results: sharedResults } = await db
    .prepare(
      `SELECT b.user_id AS uid, COUNT(*) AS n
       FROM server_members a JOIN server_members b ON a.server_id = b.server_id
       WHERE a.user_id = ? AND b.user_id IN (${placeholders})
       GROUP BY b.user_id`,
    )
    .bind(userId, ...friends.map((f) => f.id))
    .all();
  const shared = new Map<string, number>();
  for (const row of sharedResults as unknown as SharedCountRow[]) {
    shared.set(row.uid, row.n);
  }
  return friends.map((f) => ({ user: rowToUser(f), sharedServers: shared.get(f.id) ?? 0 }));
}

export async function friendshipExists(db: D1Database, userId: string, otherId: string): Promise<boolean> {
  const row = await db
    .prepare(
      "SELECT 1 FROM friendships WHERE (user_id = ? AND friend_id = ?) OR (user_id = ? AND friend_id = ?)",
    )
    .bind(userId, otherId, otherId, userId)
    .first();
  return row !== null;
}

export async function createFriendRequest(
  db: D1Database,
  args: { id: string; userId: string; friendId: string },
): Promise<void> {
  await db
    .prepare(
      "INSERT INTO friendships (id, user_id, friend_id, status, requested_by) VALUES (?, ?, ?, 'pending', ?)",
    )
    .bind(args.id, args.userId, args.friendId, args.userId)
    .run();
}

export async function getFriendRequest(db: D1Database, id: string): Promise<FriendshipRow | null> {
  return (await db.prepare("SELECT * FROM friendships WHERE id = ?").bind(id).first()) as
    | FriendshipRow
    | null;
}

export async function acceptFriendRequest(db: D1Database, id: string): Promise<void> {
  await db
    .prepare("UPDATE friendships SET status = 'accepted' WHERE id = ? AND status = 'pending'")
    .bind(id)
    .run();
}

/** Remove a pending/declined request row (and never resurrect an accepted one). */
export async function deleteFriendRequest(db: D1Database, id: string): Promise<void> {
  await db
    .prepare("DELETE FROM friendships WHERE id = ? AND status != 'accepted'")
    .bind(id)
    .run();
}

export async function listPendingRequests(
  db: D1Database,
  userId: string,
): Promise<FriendshipRequest[]> {  const { results: incomingResults } = await db
    .prepare(
      `SELECT f.id, f.created_at, u.id AS uid, u.username, u.last_seen, u.created_at AS ucreated
       FROM friendships f JOIN users u ON u.id = f.user_id
       WHERE f.friend_id = ? AND f.status = 'pending'`,
    )
    .bind(userId)
    .all();
  const { results: outgoingResults } = await db
    .prepare(
      `SELECT f.id, f.created_at, u.id AS uid, u.username, u.last_seen, u.created_at AS ucreated
       FROM friendships f JOIN users u ON u.id = f.friend_id
       WHERE f.user_id = ? AND f.status = 'pending'`,
    )
    .bind(userId)
    .all();

  const toRequest = (r: FriendRequestRow, direction: "incoming" | "outgoing"): FriendshipRequest => ({
    id: r.id,
    user: { id: r.uid, username: r.username, lastSeen: r.last_seen, createdAt: r.ucreated },
    direction,
    createdAt: r.created_at,
  });
  const incoming = (incomingResults as unknown as FriendRequestRow[]).map((r) => toRequest(r, "incoming"));
  const outgoing = (outgoingResults as unknown as FriendRequestRow[]).map((r) => toRequest(r, "outgoing"));
  return [...incoming, ...outgoing];
}

/**
 * Remove an accepted friendship in both directions. The schema stores one
 * direction per row (UNIQUE(user_id, friend_id)); accept creates both rows,
 * so delete both.
 */
export async function removeFriendship(db: D1Database, userId: string, friendId: string): Promise<void> {
  await db
    .prepare(
      `DELETE FROM friendships
       WHERE (user_id = ? AND friend_id = ? AND status = 'accepted')
          OR (user_id = ? AND friend_id = ? AND status = 'accepted')`,
    )
    .bind(userId, friendId, friendId, userId)
    .run();
}

// ---------------------------------------------------------------------------
// Fase 4 — OAuth users
// ---------------------------------------------------------------------------

export async function getUserByOAuth(
  db: D1Database,
  provider: string,
  oauthId: string,
): Promise<UserRow | null> {
  return (await db
    .prepare("SELECT * FROM users WHERE oauth_provider = ? AND oauth_id = ?")
    .bind(provider, oauthId)
    .first()) as UserRow | null;
}

export async function createOAuthUser(
  db: D1Database,
  args: { id: string; username: string; email?: string; oauthProvider: string; oauthId: string },
): Promise<UserRow> {
  const now = new Date().toISOString();
  // Deterministic placeholder credentials: OAuth users never log in with a
  // password (login only checks password when it exists).
  await db
    .prepare(
      `INSERT INTO users (id, username, password_salt, password_hash, email, oauth_provider, oauth_id)
       VALUES (?, ?, 'oauth', 'oauth', ?, ?, ?)`,
    )
    .bind(args.id, args.username, args.email ?? null, args.oauthProvider, args.oauthId)
    .run();
  return {
    id: args.id,
    username: args.username,
    password_salt: "oauth",
    password_hash: "oauth",
    last_seen: now,
    created_at: now,
    email: args.email,
    oauth_provider: args.oauthProvider,
    oauth_id: args.oauthId,
  };
}

// ---------------------------------------------------------------------------
// Fase 5 — moderación (bans, blocks, reports, ownership)
// ---------------------------------------------------------------------------

export interface BanRow {
  server_id: string;
  user_id: string;
  reason: string | null;
  banned_by: string;
  created_at: string;
  username: string;
}

export async function addBan(
  db: D1Database,
  serverId: string,
  userId: string,
  reason: string | null,
  bannedBy: string,
): Promise<void> {
  await db
    .prepare(
      `INSERT INTO server_bans (server_id, user_id, reason, banned_by)
       VALUES (?, ?, ?, ?)
       ON CONFLICT (server_id, user_id) DO UPDATE SET reason = excluded.reason`,
    )
    .bind(serverId, userId, reason, bannedBy)
    .run();
}

export async function removeBan(db: D1Database, serverId: string, userId: string): Promise<void> {
  await db
    .prepare("DELETE FROM server_bans WHERE server_id = ? AND user_id = ?")
    .bind(serverId, userId)
    .run();
}

export async function isBanned(db: D1Database, serverId: string, userId: string): Promise<boolean> {
  const row = await db
    .prepare("SELECT 1 FROM server_bans WHERE server_id = ? AND user_id = ?")
    .bind(serverId, userId)
    .first();
  return row !== null;
}

export async function listBans(db: D1Database, serverId: string): Promise<BanRow[]> {
  const { results } = await db
    .prepare(
      `SELECT b.server_id, b.user_id, b.reason, b.banned_by, b.created_at, u.username
       FROM server_bans b JOIN users u ON u.id = b.user_id
       WHERE b.server_id = ? ORDER BY b.created_at DESC`,
    )
    .bind(serverId)
    .all<BanRow & { username: string }>();
  return results as unknown as BanRow[];
}

export async function addBlock(db: D1Database, userId: string, blockedId: string): Promise<void> {
  await db
    .prepare("INSERT OR IGNORE INTO blocks (user_id, blocked_id) VALUES (?, ?)")
    .bind(userId, blockedId)
    .run();
}

export async function removeBlock(db: D1Database, userId: string, blockedId: string): Promise<void> {
  await db
    .prepare("DELETE FROM blocks WHERE user_id = ? AND blocked_id = ?")
    .bind(userId, blockedId)
    .run();
}

/** True when `a` blocked `b` (either direction counts for DMs/requests). */
export async function isBlocked(db: D1Database, a: string, b: string): Promise<boolean> {
  const row = await db
    .prepare(
      `SELECT 1 FROM blocks WHERE (user_id = ? AND blocked_id = ?) OR (user_id = ? AND blocked_id = ?)`,
    )
    .bind(a, b, b, a)
    .first();
  return row !== null;
}

export async function createReport(
  db: D1Database,
  args: { id: string; reporterId: string; targetType: string; targetId: string; reason?: string },
): Promise<void> {
  await db
    .prepare(
      `INSERT INTO reports (id, reporter_id, target_type, target_id, reason)
       VALUES (?, ?, ?, ?, ?)`,
    )
    .bind(args.id, args.reporterId, args.targetType, args.targetId, args.reason ?? null)
    .run();
}

export async function transferOwnership(db: D1Database, serverId: string, newOwnerId: string): Promise<void> {
  await db
    .prepare("UPDATE servers SET owner_id = ? WHERE id = ?")
    .bind(newOwnerId, serverId)
    .run();
}

/** True when the server's owner account is soft-deleted (orphan server). */
export async function isOwnerSoftDeleted(db: D1Database, serverId: string): Promise<boolean> {
  const row = await db
    .prepare(
      `SELECT 1 FROM servers s JOIN users u ON u.id = s.owner_id
       WHERE s.id = ? AND u.deleted_at IS NOT NULL`,
    )
    .bind(serverId)
    .first();
  return row !== null;
}
