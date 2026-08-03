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
}

export interface ServerRow {
  id: string;
  name: string;
  owner_id: string;
  invite_code: string;
  created_at: string;
}

export interface ChannelRow {
  id: string;
  server_id: string;
  name: string;
  kind: "text" | "voice";
  created_at: string;
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
  return { id: r.id, name: r.name, ownerId: r.owner_id, inviteCode: r.invite_code, createdAt: r.created_at };
}

export function rowToChannel(r: ChannelRow): Channel {
  return { id: r.id, serverId: r.server_id, name: r.name, kind: r.kind, createdAt: r.created_at };
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
  const { results } = await db
    .prepare("SELECT * FROM channels WHERE server_id = ? ORDER BY created_at ASC")
    .bind(serverId)
    .all();
  return (results as unknown as ChannelRow[]).map(rowToChannel);
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
    const list = channelsByServer.get(row.server_id) ?? [];
    list.push(rowToChannel(row));
    channelsByServer.set(row.server_id, list);
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
      `SELECT m.id, m.channel_id, m.author_id, m.content, m.created_at, u.username
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
    username: string;
  }>).map((r) => ({
    id: r.id,
    channelId: r.channel_id,
    authorId: r.author_id,
    authorName: r.username,
    content: r.content,
    createdAt: r.created_at,
  }));
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

export async function listPendingRequests(
  db: D1Database,
  userId: string,
): Promise<FriendshipRequest[]> {
  const { results: incomingResults } = await db
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
