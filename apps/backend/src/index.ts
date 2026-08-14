import type { BufferedMessage, TextMessage, User } from "@lumen/protocol";

import { ApiError, Router } from "./router";
import * as auth from "./auth";
import * as db from "./db";
import { enforceRateLimit } from "./rate-limit";
import { getRealtimeConfig } from "./realtime";
import {
  generateInviteCode,
  validateChannelKind,
  validateChannelName,
  validatePassword,
  validateServerName,
  validateUsername,
} from "./validation";

export { LumenChannelDO } from "./do/ChannelDO";
export { PresenceHubDO } from "./do/PresenceHubDO";

/**
 * CORS: only registered origins get headers (Fase 1 security, ADR-0009).
 * Native clients (Slint/curl) send no Origin — those requests are NOT CORS
 * requests and must pass untouched (the JWT in Authorization is the actual
 * protection, not CORS). Browsers with an unregistered origin get no headers
 * and the browser blocks the response.
 */
const ALLOWED_ORIGINS = new Set([
  "http://localhost:8787", // dev (wrangler)
  "http://localhost:5173", // dev (web client / vite)
  "https://app.lumen.chat", // production web client (ajustar al dominio real)
]);

function corsHeaders(request: Request): Record<string, string> {
  const origin = request.headers.get("origin");
  if (!origin) return {};
  if (ALLOWED_ORIGINS.has(origin)) {
    return {
      "access-control-allow-origin": origin,
      "access-control-allow-methods": "GET, POST, PUT, PATCH, DELETE, OPTIONS",
      "access-control-allow-headers": "authorization, content-type",
      "access-control-max-age": "86400",
      "vary": "origin",
    };
  }
  return {}; // sin headers → el browser bloquea
}

/** Maximum accepted request body (Fase 1 hardening). */
const MAX_BODY = 1024 * 1024; // 1 MB

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/** json() + CORS headers for the given request (used by the middleware). */
function jsonCors(body: unknown, status: number, request: Request): Response {
  const res = json(body, status);
  const headers = new Headers(res.headers);
  for (const [k, v] of Object.entries(corsHeaders(request))) headers.set(k, v);
  return new Response(res.body, { status: res.status, headers });
}

/**
 * Read and parse a JSON body with a hard size cap. `content-length` is
 * checked early in the handler (cheap); here the actual body is read so
 * chunked/oversized bodies are caught too.
 */
async function readJson(request: Request): Promise<Record<string, unknown>> {
  const text = await request.text();
  if (text.length > MAX_BODY) {
    throw new ApiError(413, "payload_too_large", "request body must be ≤ 1 MB");
  }
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    throw new ApiError(422, "invalid_json", "request body must be valid JSON");
  }
  if (typeof body !== "object" || body === null || Array.isArray(body)) {
    throw new ApiError(422, "invalid_json", "request body must be a JSON object");
  }
  return body as Record<string, unknown>;
}

/** Channel access: server channels require membership, DM channels require
 *  being one of the two participants. */
async function canAccessChannel(
  dbc: D1Database,
  channel: { id: string; kind: string; server_id: string | null },
  userId: string,
): Promise<boolean> {
  if (channel.kind === "dm") {
    return await db.isDmMember(dbc, channel.id, userId);
  }
  return await db.isMember(dbc, channel.server_id!, userId);
}

async function requireUser(request: Request, env: Env): Promise<User> {
  const header = request.headers.get("authorization");
  // Browser WebSocket cannot set the Authorization header, so the WS upgrade
  // also accepts the token as ?token= (HMAC-signed, short-lived — documented
  // trade-off in docs/protocol.md §1).
  const token = header?.startsWith("Bearer ")
    ? header.slice("Bearer ".length).trim()
    : new URL(request.url).searchParams.get("token") ?? "";
  if (!token) throw new ApiError(401, "unauthorized");
  let payload: auth.JwtPayload;
  try {
    payload = await auth.verifyToken(token, auth.getSecret(env));
  } catch {
    throw new ApiError(401, "unauthorized", "invalid or expired token");
  }
  const row = await db.getUserById(env.LUMEN_D1, payload.sub);
  if (!row || row.deleted_at) throw new ApiError(401, "unauthorized");
  return db.rowToUser(row);
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

const router = new Router();

// auth
router.post("/api/auth/register", false, async (ctx) => {
  const body = await readJson(ctx.request);
  if (!validateUsername(body.username)) {
    throw new ApiError(422, "invalid_username", "username must be 3-32 chars [A-Za-z0-9_]");
  }
  if (!validatePassword(body.password)) {
    throw new ApiError(422, "invalid_password", "password must be at least 8 chars");
  }
  const existing = await db.getUserByUsername(ctx.env.LUMEN_D1, body.username);
  if (existing) throw new ApiError(409, "username_taken");
  const { salt, hash } = await auth.hashPassword(body.password);
  const id = crypto.randomUUID();
  try {
    await db.createUser(ctx.env.LUMEN_D1, { id, username: body.username, salt, hash });
  } catch {
    throw new ApiError(409, "username_taken"); // lost a race on the UNIQUE column
  }
  const now = new Date().toISOString();
  const user = db.rowToUser({
    id,
    username: body.username,
    password_salt: salt,
    password_hash: hash,
    last_seen: now,
    created_at: now,
  });
  const secret = auth.getSecret(ctx.env);
  const token = await auth.signToken(user.id, secret);
  const refreshToken = await auth.createRefreshToken(ctx.env.LUMEN_D1, user.id);
  return json({ token, refreshToken, user }, 201);
});

router.post("/api/auth/login", false, async (ctx) => {
  const body = await readJson(ctx.request);
  const username = body.username;
  const password = body.password;
  if (typeof username !== "string" || typeof password !== "string") {
    throw new ApiError(422, "missing_credentials");
  }
  const user = await db.getUserByUsername(ctx.env.LUMEN_D1, username);
  if (!user) throw new ApiError(401, "invalid_credentials");
  if (user.deleted_at) throw new ApiError(401, "invalid_credentials");
  const ok = await auth.verifyPassword(password, `${user.password_salt}:${user.password_hash}`);
  if (!ok) throw new ApiError(401, "invalid_credentials");
  await db.updateLastSeen(ctx.env.LUMEN_D1, user.id);
  const secret = auth.getSecret(ctx.env);
  const token = await auth.signToken(user.id, secret);
  const refreshToken = await auth.createRefreshToken(ctx.env.LUMEN_D1, user.id);
  return json({ token, refreshToken, user: db.rowToUser(user) }, 200);
});

// refresh token rotation (ADR-0007): revoke the presented token, issue a new
// one. Reuse of a rotated token → 401 (compromise signal).
router.post("/api/auth/refresh", false, async (ctx) => {
  const body = await readJson(ctx.request);
  const refreshToken = body.refreshToken;
  if (typeof refreshToken !== "string" || refreshToken.length === 0) {
    throw new ApiError(422, "missing_refresh_token");
  }
  const rotated = await auth.rotateRefreshToken(ctx.env.LUMEN_D1, refreshToken);
  if (!rotated) throw new ApiError(401, "invalid_refresh_token");
  const secret = auth.getSecret(ctx.env);
  return json({
    token: await auth.signToken(rotated.userId, secret),
    refreshToken: rotated.newToken,
  });
});

router.post("/api/auth/logout", false, async (ctx) => {
  const body = await readJson(ctx.request);
  const refreshToken = body.refreshToken;
  if (typeof refreshToken === "string" && refreshToken.length > 0) {
    await auth.revokeRefreshToken(ctx.env.LUMEN_D1, refreshToken);
  }
  return json({ ok: true });
});

// logout all sessions
router.delete("/api/auth/sessions", true, async (ctx) => {
  await auth.revokeAllSessions(ctx.env.LUMEN_D1, ctx.user.id);
  return json({ ok: true });
});

router.get("/api/health", false, async (ctx) => {
  const d1 = await ctx.env.LUMEN_D1.prepare("SELECT 1").first().catch(() => null);
  return json({
    status: d1 ? "ok" : "degraded",
    d1: d1 ? "ok" : "error",
    version: "2.0.0",
    timestamp: new Date().toISOString(),
  });
});

router.get("/api/me", true, async (ctx) => json({ user: ctx.user }));

// servers
router.post("/api/servers", true, async (ctx) => {
  const body = await readJson(ctx.request);
  const name = typeof body.name === "string" ? body.name.trim() : "";
  if (!validateServerName(name)) {
    throw new ApiError(422, "invalid_name", "server name must be 1-100 chars");
  }
  for (let attempt = 0; attempt < 5; attempt++) {
    try {
      const result = await db.createServerWithDefaults(ctx.env.LUMEN_D1, {
        id: crypto.randomUUID(),
        name,
        ownerId: ctx.user.id,
        inviteCode: generateInviteCode(),
      });
      return json(result, 201);
    } catch {
      // invite_code UNIQUE collision → regenerate and retry
    }
  }
  throw new ApiError(500, "server_creation_failed");
});

router.get("/api/servers", true, async (ctx) => {
  return json(await db.listServersForUser(ctx.env.LUMEN_D1, ctx.user.id));
});

// join by invite code — no server id in the path (the code addresses the server)
router.post("/api/servers/join", true, async (ctx) => {
  const body = await readJson(ctx.request);
  const inviteCode = typeof body.inviteCode === "string" ? body.inviteCode : "";
  const server = await db.getServerByInviteCode(ctx.env.LUMEN_D1, inviteCode);
  if (!server) throw new ApiError(404, "invite_not_found");
  // Ban bloquea el join por invite (Fase 5).
  if (await db.isBanned(ctx.env.LUMEN_D1, server.id, ctx.user.id)) {
    throw new ApiError(403, "banned", "you are banned from this server");
  }
  await db.addMember(ctx.env.LUMEN_D1, server.id, ctx.user.id);
  const channels = await db.getChannelsForServer(ctx.env.LUMEN_D1, server.id);
  return json({ server: db.rowToServer(server), channels });
});

router.get("/api/servers/:id", true, async (ctx, params) => {
  const server = await db.getServer(ctx.env.LUMEN_D1, params.id!);
  if (!server) throw new ApiError(404, "not_found");
  if (!(await db.isMember(ctx.env.LUMEN_D1, server.id, ctx.user.id))) {
    throw new ApiError(403, "forbidden");
  }
  const channels = await db.getChannelsForServer(ctx.env.LUMEN_D1, server.id);
  const members = await db.getMembers(ctx.env.LUMEN_D1, server.id);
  return json({ server: db.rowToServer(server), channels, members });
});

router.post("/api/servers/:id/channels", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const server = await db.getServer(dbc, params.id!);
  if (!server) throw new ApiError(404, "not_found");
  if (server.owner_id !== ctx.user.id) {
    throw new ApiError(403, "forbidden", "only the owner can create channels");
  }
  const body = await readJson(ctx.request);
  const name = typeof body.name === "string" ? body.name.trim() : "";
  if (!validateChannelName(name)) {
    throw new ApiError(422, "invalid_name", "channel name must be 1-50 chars");
  }
  if (!validateChannelKind(body.kind)) {
    throw new ApiError(422, "invalid_kind", "kind must be text or voice");
  }
  const channel = await db.createChannel(dbc, {
    id: crypto.randomUUID(),
    serverId: server.id,
    name,
    kind: body.kind,
  });
  return json({ channel }, 201);
});

// DMs (1:1 channels of kind 'dm' — text + voice in one channel)
router.post("/api/dms", true, async (ctx) => {
  const dbc = ctx.env.LUMEN_D1;
  const body = await readJson(ctx.request);
  const username = typeof body.username === "string" ? body.username.trim() : "";
  if (!validateUsername(username)) {
    throw new ApiError(422, "invalid_username", "username must be 3-32 chars [A-Za-z0-9_]");
  }
  const target = await db.getUserByUsername(dbc, username);
  if (!target) throw new ApiError(404, "user_not_found");
  if (target.id === ctx.user.id) throw new ApiError(422, "cannot_dm_self");
  if (await db.isBlocked(dbc, ctx.user.id, target.id)) {
    throw new ApiError(403, "blocked", "this user is blocked");
  }
  if (!(await db.friendshipExists(dbc, ctx.user.id, target.id))) {
    throw new ApiError(403, "not_friends", "you can only DM friends");
  }
  const existing = await db.getDmChannelBetween(dbc, ctx.user.id, target.id);
  let channel: db.ChannelRow;
  let created = false;
  if (existing) {
    // Re-open path: I soft-deleted this DM before → restore my membership.
    await db.restoreDmMember(dbc, existing.id, ctx.user.id);
    channel = existing;
  } else {
    channel = await db.createDmChannel(dbc, crypto.randomUUID(), ctx.user.id, target.id);
    created = true;
  }
  return json(
    { channel: db.rowToChannel(channel), otherUsername: target.username },
    created ? 201 : 200,
  );
});

router.get("/api/dms", true, async (ctx) => {
  const rows = await db.listDmChannelsForUser(ctx.env.LUMEN_D1, ctx.user.id);
  return json(rows.map((r) => ({ channel: db.rowToChannel(r.channel), otherUsername: r.otherUsername })));
});

// messages — READ-ONLY (ADR-0010): send/edit/delete flow through the
// PresenceHubDO WS (`chat` / `chat-edit` / `chat-delete`). This route
// combines D1 blocks (cursor `before=<lastAt>,<id>`) + the hub's pending
// buffer on the newest page.
router.get("/api/channels/:id/messages", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const channel = await db.getChannel(dbc, params.id!);
  if (!channel) throw new ApiError(404, "not_found");
  if (!(await canAccessChannel(dbc, channel, ctx.user.id))) {
    throw new ApiError(403, "forbidden");
  }
  const before = ctx.url.searchParams.get("before");
  const [beforeAt, beforeId] = before?.split(",") ?? [];
  const blocks = await db.getMessageBlocks(dbc, channel.id, beforeAt, beforeId);
  let msgs: TextMessage[] = blocks.flatMap((b) => db.blockMessagesToText(b.messages, channel.id));

  // Merge the hub's pending buffer only on the newest page (no cursor):
  // unflushed messages are newer than every flushed block.
  if (!before) {
    const hubId = ctx.env.LUMEN_PRESENCE_DO.idFromName("hub");
    const hub = ctx.env.LUMEN_PRESENCE_DO.get(hubId);
    const res = await hub.fetch(`https://do/buffer/${encodeURIComponent(channel.id)}`);
    if (res.ok) {
      const buffered = (await res.json()) as BufferedMessage[];
      const mapped: TextMessage[] = buffered.map((b) => ({
        id: b.id,
        channelId: channel.id,
        authorId: b.authorId,
        authorName: b.authorName,
        content: b.content,
        createdAt: b.createdAt,
        ...(b.editedAt ? { editedAt: b.editedAt } : {}),
        ...(b.deletedAt ? { deletedAt: b.deletedAt } : {}),
        ...(b.replyTo ? { replyTo: b.replyTo } : {}),
        ...(b.attachmentUrl ? { attachmentUrl: b.attachmentUrl } : {}),
      }));
      msgs = [...msgs, ...mapped];
      msgs.sort((a, b) => a.createdAt.localeCompare(b.createdAt) || a.id.localeCompare(b.id));
    }
  }
  return json(msgs.slice(-100));
});

// Fase 3 — presence WebSocket upgrade → PresenceHubDO (singleton "hub").
// The Worker validates the JWT + membership and hands the DO the resolved
// lists; the DO never re-validates the upgrade (it does re-validate each
// chat/voice-join, R4).
router.get("/api/presence", true, async (ctx) => {
  const data = await db.getUserPresenceContext(ctx.env.LUMEN_D1, ctx.user.id);
  const url = new URL(ctx.request.url);
  url.searchParams.set("userId", ctx.user.id);
  url.searchParams.set("username", ctx.user.username);
  url.searchParams.set("servers", data.servers.join(","));
  url.searchParams.set("friends", data.friends.join(","));
  const id = ctx.env.LUMEN_PRESENCE_DO.idFromName("hub");
  const stub = ctx.env.LUMEN_PRESENCE_DO.get(id);
  return await stub.fetch(new Request(url.toString(), ctx.request));
});

// friends
router.get("/api/friends", true, async (ctx) => {
  const dbc = ctx.env.LUMEN_D1;
  const friends = await db.listFriends(dbc, ctx.user.id);
  const pending = await db.listPendingRequests(dbc, ctx.user.id);
  return json({ friends, pending });
});

router.post("/api/friends/requests", true, async (ctx) => {
  const dbc = ctx.env.LUMEN_D1;
  const body = await readJson(ctx.request);
  const username = typeof body.username === "string" ? body.username : "";
  if (!validateUsername(username)) {
    throw new ApiError(422, "invalid_username", "username must be 3-32 chars [A-Za-z0-9_]");
  }
  const target = await db.getUserByUsername(dbc, username);
  if (!target) throw new ApiError(404, "user_not_found");
  if (target.id === ctx.user.id) throw new ApiError(422, "cannot_add_self");
  // Block (Fase 5): ni friend requests ni DMs con alguien que te bloqueó.
  if (await db.isBlocked(dbc, ctx.user.id, target.id)) {
    throw new ApiError(403, "blocked", "this user is blocked");
  }
  if (await db.friendshipExists(dbc, ctx.user.id, target.id)) {
    throw new ApiError(409, "already_requested");
  }
  const requestId = crypto.randomUUID();
  try {
    await db.createFriendRequest(dbc, { id: requestId, userId: ctx.user.id, friendId: target.id });
  } catch {
    throw new ApiError(409, "already_requested"); // concurrent duplicate
  }
  return json(
    {
      request: {
        id: requestId,
        user: db.rowToUser(target),
        direction: "outgoing",
        createdAt: new Date().toISOString(),
      },
    },
    201,
  );
});

router.post("/api/friends/requests/:id/accept", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const request = await db.getFriendRequest(dbc, params.id!);
  if (!request) throw new ApiError(404, "not_found");
  if (request.friend_id !== ctx.user.id) {
    throw new ApiError(403, "forbidden", "only the recipient can accept");
  }
  if (request.status !== "pending") throw new ApiError(409, "already_handled");
  await db.acceptFriendRequest(dbc, request.id);
  const friend = await db.getUserById(dbc, request.user_id);
  if (!friend) throw new ApiError(404, "not_found");
  return json({ friend: db.rowToUser(friend) });
});

router.delete("/api/friends/requests/:id", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const request = await db.getFriendRequest(dbc, params.id!);
  if (!request) throw new ApiError(404, "not_found");
  if (request.friend_id !== ctx.user.id) {
    throw new ApiError(403, "forbidden", "only the recipient can decline");
  }
  await db.deleteFriendRequest(dbc, request.id);
  return json({ ok: true });
});

// realtime
router.get("/api/realtime/config", true, async (ctx) => {
  return json(await getRealtimeConfig(ctx.env));
});

// WebSocket upgrade → LumenChannelDO (validates membership, then delegates)
router.get("/api/ws/:channelId", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const channel = await db.getChannel(dbc, params.channelId!);
  if (!channel) throw new ApiError(404, "not_found");
  if (!(await canAccessChannel(dbc, channel, ctx.user.id))) {
    throw new ApiError(403, "forbidden");
  }
  const url = new URL(ctx.request.url);
  url.searchParams.set("channelId", channel.id);
  url.searchParams.set("userId", ctx.user.id);
  const id = ctx.env.LUMEN_CHANNEL_DO.idFromName(`lumen-${channel.id}`);
  const stub = ctx.env.LUMEN_CHANNEL_DO.get(id);
  return await stub.fetch(new Request(url.toString(), ctx.request));
});

// ---------------------------------------------------------------------------
// Fase 2 — CRUD (profile, servers, channels, messages, friends, DMs)
// ---------------------------------------------------------------------------

// profile
router.patch("/api/me", true, async (ctx) => {
  const dbc = ctx.env.LUMEN_D1;
  const body = await readJson(ctx.request);
  const username = body.username;
  if (username !== undefined) {
    if (typeof username !== "string" || !validateUsername(username)) {
      throw new ApiError(422, "invalid_username", "username must be 3-32 chars [A-Za-z0-9_]");
    }
    const existing = await db.getUserByUsername(dbc, username);
    if (existing && existing.id !== ctx.user.id) throw new ApiError(409, "username_taken");
    try {
      await db.updateUsername(dbc, ctx.user.id, username);
    } catch {
      throw new ApiError(409, "username_taken"); // lost a race on the UNIQUE column
    }
  }
  const row = await db.getUserById(dbc, ctx.user.id);
  if (!row) throw new ApiError(404, "not_found");
  return json({ user: db.rowToUser(row) });
});

router.put("/api/me/password", true, async (ctx) => {
  const dbc = ctx.env.LUMEN_D1;
  const body = await readJson(ctx.request);
  const current = body.currentPassword;
  const next = body.newPassword;
  if (typeof current !== "string" || typeof next !== "string") {
    throw new ApiError(422, "missing_credentials");
  }
  const row = await db.getUserById(dbc, ctx.user.id);
  if (!row) throw new ApiError(404, "not_found");
  const ok = await auth.verifyPassword(current, `${row.password_salt}:${row.password_hash}`);
  if (!ok) throw new ApiError(403, "invalid_password", "current password is wrong");
  if (!validatePassword(next)) {
    throw new ApiError(422, "invalid_password", "password must be at least 8 chars");
  }
  const { salt, hash } = await auth.hashPassword(next);
  await db.updatePassword(dbc, ctx.user.id, salt, hash);
  return new Response(null, { status: 204 });
});

// soft delete account + revoke every session
router.delete("/api/me", true, async (ctx) => {
  await db.softDeleteUser(ctx.env.LUMEN_D1, ctx.user.id);
  await auth.revokeAllSessions(ctx.env.LUMEN_D1, ctx.user.id);
  return new Response(null, { status: 204 });
});

// servers
router.patch("/api/servers/:id", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const server = await db.getServer(dbc, params.id!);
  if (!server) throw new ApiError(404, "not_found");
  if (server.owner_id !== ctx.user.id) {
    throw new ApiError(403, "forbidden", "only the owner can edit the server");
  }
  const body = await readJson(ctx.request);
  const patch: { name?: string; icon?: string } = {};
  if (body.name !== undefined) {
    if (typeof body.name !== "string" || !validateServerName(body.name)) {
      throw new ApiError(422, "invalid_name", "server name must be 1-100 chars");
    }
    patch.name = body.name.trim();
  }
  if (body.icon !== undefined) {
    if (typeof body.icon !== "string") throw new ApiError(422, "invalid_icon");
    patch.icon = body.icon;
  }
  const updated = await db.updateServer(dbc, server.id, patch);
  return json({ server: db.rowToServer(updated!) });
});

router.delete("/api/servers/:id", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  if (ctx.url.searchParams.get("confirm") !== "true") {
    throw new ApiError(400, "confirm_required", "pass ?confirm=true to delete a server");
  }
  const server = await db.getServer(dbc, params.id!);
  if (!server) throw new ApiError(404, "not_found");
  if (server.owner_id !== ctx.user.id) {
    throw new ApiError(403, "forbidden", "only the owner can delete the server");
  }
  await db.deleteServer(dbc, server.id);
  return new Response(null, { status: 204 });
});

router.post("/api/servers/:id/leave", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const server = await db.getServer(dbc, params.id!);
  if (!server) throw new ApiError(404, "not_found");
  if (server.owner_id === ctx.user.id) {
    throw new ApiError(403, "forbidden", "the owner cannot leave — transfer or delete the server");
  }
  await db.removeMember(dbc, server.id, ctx.user.id);
  return new Response(null, { status: 204 });
});

router.post("/api/servers/:id/invite", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const server = await db.getServer(dbc, params.id!);
  if (!server) throw new ApiError(404, "not_found");
  if (server.owner_id !== ctx.user.id) {
    throw new ApiError(403, "forbidden", "only the owner can regenerate the invite");
  }
  for (let attempt = 0; attempt < 5; attempt++) {
    const code = generateInviteCode();
    try {
      await db.regenerateInvite(dbc, server.id, code);
      return json({ inviteCode: code });
    } catch {
      // invite_code UNIQUE collision → retry with a fresh code
    }
  }
  throw new ApiError(500, "invite_regeneration_failed");
});

router.delete("/api/servers/:id/members/:userId", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const server = await db.getServer(dbc, params.id!);
  if (!server) throw new ApiError(404, "not_found");
  if (server.owner_id !== ctx.user.id) {
    throw new ApiError(403, "forbidden", "only the owner can kick members");
  }
  if (params.userId === server.owner_id) {
    throw new ApiError(422, "cannot_kick_owner");
  }
  await db.removeMember(dbc, server.id, params.userId!);
  return new Response(null, { status: 204 });
});

// channels
router.patch("/api/channels/:id", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const channel = await db.getChannel(dbc, params.id!);
  if (!channel) throw new ApiError(404, "not_found");
  if (!channel.server_id) throw new ApiError(422, "cannot_edit_dm");
  const server = await db.getServer(dbc, channel.server_id);
  if (!server || server.owner_id !== ctx.user.id) {
    throw new ApiError(403, "forbidden", "only the server owner can edit channels");
  }
  const body = await readJson(ctx.request);
  const patch: { name?: string; topic?: string | null; position?: number } = {};
  if (body.name !== undefined) {
    if (typeof body.name !== "string" || !validateChannelName(body.name)) {
      throw new ApiError(422, "invalid_name", "channel name must be 1-50 chars");
    }
    patch.name = body.name.trim();
  }
  if (body.topic !== undefined) {
    if (body.topic !== null && (typeof body.topic !== "string" || body.topic.length > 2000)) {
      throw new ApiError(422, "invalid_topic", "topic must be ≤ 2000 chars");
    }
    patch.topic = body.topic;
  }
  if (body.position !== undefined) {
    if (typeof body.position !== "number" || !Number.isInteger(body.position)) {
      throw new ApiError(422, "invalid_position");
    }
    patch.position = body.position;
  }
  const updated = await db.updateChannel(dbc, channel.id, patch);
  return json({ channel: db.rowToChannel(updated!) });
});

router.delete("/api/channels/:id", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const channel = await db.getChannel(dbc, params.id!);
  if (!channel) throw new ApiError(404, "not_found");
  if (!channel.server_id) throw new ApiError(422, "cannot_delete_dm");
  const server = await db.getServer(dbc, channel.server_id);
  if (!server || server.owner_id !== ctx.user.id) {
    throw new ApiError(403, "forbidden", "only the server owner can delete channels");
  }
  await db.deleteChannel(dbc, channel.id);
  return new Response(null, { status: 204 });
});

// messages — READ-ONLY (ADR-0010): send/edit/delete flow through the
// PresenceHubDO WS (`chat` / `chat-edit` / `chat-delete`). This route
// combines D1 blocks (cursor `before=<lastAt>,<id>`) + the hub's pending
// buffer on the newest page.
router.delete("/api/friends/:userId", true, async (ctx, params) => {
  await db.removeFriendship(ctx.env.LUMEN_D1, ctx.user.id, params.userId!);
  return new Response(null, { status: 204 });
});

// DMs — soft per-user delete (the other participant keeps the history)
router.delete("/api/dms/:id", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const channel = await db.getChannel(dbc, params.id!);
  if (!channel || channel.kind !== "dm") throw new ApiError(404, "not_found");
  if (!(await db.isDmMember(dbc, channel.id, ctx.user.id))) {
    throw new ApiError(403, "forbidden");
  }
  await db.removeDmMember(dbc, channel.id, ctx.user.id);
  return new Response(null, { status: 204 });
});

// ---------------------------------------------------------------------------
// Fase 4 — OAuth (Google/GitHub) + R2 assets
// ---------------------------------------------------------------------------

/** Dynamic env access for `GOOGLE_CLIENT_ID` etc. (template-literal indexing
 *  doesn't typecheck against Env). */
function oauthSecret(env: Env, provider: string, suffix: "CLIENT_ID" | "CLIENT_SECRET"): string {
  const key = `${provider.toUpperCase()}_${suffix}`;
  return (env as unknown as Record<string, string | undefined>)[key] ?? "";
}

const OAUTH_PROVIDERS = {  google: {
    authUrl: "https://accounts.google.com/o/oauth2/v2/auth",
    tokenUrl: "https://oauth2.googleapis.com/token",
    userInfoUrl: "https://www.googleapis.com/oauth2/v2/userinfo",
    scope: "openid email profile",
    idField: "sub",
    nameField: "name",
  },
  github: {
    authUrl: "https://github.com/login/oauth/authorize",
    tokenUrl: "https://github.com/login/oauth/access_token",
    userInfoUrl: "https://api.github.com/user",
    scope: "user:email",
    idField: "id",
    nameField: "login",
  },
} as const;

router.get("/api/oauth/:provider", false, async (ctx, params) => {
  const provider = OAUTH_PROVIDERS[params.provider as keyof typeof OAUTH_PROVIDERS];
  if (!provider) throw new ApiError(404, "not_found");
  const clientId = oauthSecret(ctx.env, params.provider!, "CLIENT_ID");
  if (!clientId) throw new ApiError(500, "server_misconfigured", `${params.provider} OAuth is not configured`);

  const state = crypto.randomUUID();
  await ctx.env.LUMEN_D1.prepare("INSERT INTO oauth_states (state, expires_at) VALUES (?, ?)")
    .bind(state, Date.now() + 600_000)
    .run();

  const qs = new URLSearchParams({
    client_id: clientId,
    redirect_uri: `${ctx.env.OAUTH_CALLBACK_URL}/${params.provider}`,
    response_type: "code",
    scope: provider.scope,
    state,
  });
  return Response.redirect(`${provider.authUrl}?${qs}`, 302);
});

router.get("/api/oauth/:provider/callback", false, async (ctx, params) => {
  const provider = OAUTH_PROVIDERS[params.provider as keyof typeof OAUTH_PROVIDERS];
  if (!provider) throw new ApiError(404, "not_found");
  const code = ctx.url.searchParams.get("code");
  const state = ctx.url.searchParams.get("state");
  if (!code || !state) throw new ApiError(400, "missing_params");

  // Anti-CSRF: consume the state in one op (delete + expiry check).
  const row = await ctx.env.LUMEN_D1.prepare(
    "DELETE FROM oauth_states WHERE state = ? AND expires_at > ? RETURNING state",
  )
    .bind(state, Date.now())
    .first();
  if (!row) throw new ApiError(400, "invalid_state");

  const clientId = oauthSecret(ctx.env, params.provider!, "CLIENT_ID");
  const clientSecret = oauthSecret(ctx.env, params.provider!, "CLIENT_SECRET");
  if (!clientId || !clientSecret) {
    throw new ApiError(500, "server_misconfigured", `${params.provider} OAuth is not configured`);
  }
  const tokenRes = await fetch(provider.tokenUrl, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded", accept: "application/json" },
    body: new URLSearchParams({
      client_id: clientId,
      client_secret: clientSecret,
      code,
      redirect_uri: `${ctx.env.OAUTH_CALLBACK_URL}/${params.provider}`,
      grant_type: "authorization_code",
    }),
  });
  if (!tokenRes.ok) throw new ApiError(502, "oauth_exchange_failed");
  const tokenJson = (await tokenRes.json()) as { access_token?: string };
  if (!tokenJson.access_token) throw new ApiError(502, "oauth_exchange_failed");

  const infoRes = await fetch(provider.userInfoUrl, {
    headers: { authorization: `Bearer ${tokenJson.access_token}`, "user-agent": "lumen-backend" },
  });
  if (!infoRes.ok) throw new ApiError(502, "oauth_userinfo_failed");
  const info = (await infoRes.json()) as Record<string, unknown>;

  const oauthId = String(info[provider.idField] ?? "");
  if (!oauthId) throw new ApiError(502, "oauth_userinfo_failed");
  const email = typeof info.email === "string" ? info.email : `${oauthId}@${params.provider}.local`;
  const rawName = info[provider.nameField] ?? email.split("@")[0] ?? "user";
  const username =
    String(rawName).replace(/[^A-Za-z0-9_]/g, "_").slice(0, 32) || `user_${oauthId.slice(0, 6)}`;

  let user = await db.getUserByOAuth(ctx.env.LUMEN_D1, params.provider!, oauthId);
  if (!user) {
    // Username collision → append a numeric suffix (retry up to 5).
    for (let attempt = 0; attempt < 5; attempt++) {
      const candidate = attempt === 0 ? username : `${username}${attempt + 1}`;
      try {
        user = await db.createOAuthUser(ctx.env.LUMEN_D1, {
          id: crypto.randomUUID(),
          username: candidate,
          email,
          oauthProvider: params.provider!,
          oauthId,
        });
        break;
      } catch {
        // UNIQUE(username) collision → next suffix
      }
    }
    if (!user) throw new ApiError(500, "user_creation_failed");
  }

  const secret = auth.getSecret(ctx.env);
  const token = await auth.signToken(user.id, secret);
  const refresh = await auth.createRefreshToken(ctx.env.LUMEN_D1, user.id);

  const client = ctx.url.searchParams.get("client") ?? "web";
  const base = client === "desktop" ? "lumen://auth/callback" : ctx.env.WEB_CLIENT_URL;
  return Response.redirect(`${base}?token=${token}&refreshToken=${refresh}`, 302);
});

// --- R2 assets ---

const MAX_AVATAR = 5 * 1024 * 1024; // 5 MB
const MAX_SERVER_ICON = 5 * 1024 * 1024;

router.put("/api/me/avatar", true, async (ctx) => {
  const size = Number(ctx.request.headers.get("content-length") ?? 0);
  if (size > MAX_AVATAR) throw new ApiError(413, "too_large", "avatar must be ≤ 5 MB");
  const blob = await ctx.request.arrayBuffer();
  if (blob.byteLength > MAX_AVATAR) throw new ApiError(413, "too_large");
  const key = `avatars/${ctx.user.id}.png`;
  await ctx.env.LUMEN_R2.put(key, blob, {
    httpMetadata: { contentType: "image/png", cacheControl: "public, max-age=86400" },
  });
  await db.updateAvatar(ctx.env.LUMEN_D1, ctx.user.id, key);
  return json({ url: `/api/assets/${key}` });
});

router.put("/api/servers/:id/icon", true, async (ctx, params) => {
  const server = await db.getServer(ctx.env.LUMEN_D1, params.id!);
  if (!server) throw new ApiError(404, "not_found");
  if (server.owner_id !== ctx.user.id) throw new ApiError(403, "forbidden");
  const size = Number(ctx.request.headers.get("content-length") ?? 0);
  if (size > MAX_SERVER_ICON) throw new ApiError(413, "too_large", "icon must be ≤ 5 MB");
  const blob = await ctx.request.arrayBuffer();
  if (blob.byteLength > MAX_SERVER_ICON) throw new ApiError(413, "too_large");
  const key = `server-icons/${server.id}.png`;
  await ctx.env.LUMEN_R2.put(key, blob, {
    httpMetadata: { contentType: "image/png", cacheControl: "public, max-age=86400" },
  });
  await db.updateServer(ctx.env.LUMEN_D1, server.id, { icon: key });
  return json({ url: `/api/assets/${key}` });
});

router.get("/api/assets/:path+", false, async (ctx, params) => {
  const obj = await ctx.env.LUMEN_R2.get(params.path!);
  if (!obj) return json({ error: "not_found" }, 404);
  return new Response(obj.body, {
    headers: {
      "content-type": obj.httpMetadata?.contentType ?? "application/octet-stream",
      "cache-control": "public, max-age=86400",
      "etag": obj.httpEtag,
    },
  });
});

// ---------------------------------------------------------------------------
// Fase 5 — moderación (bans, blocks, reports, transferencia)
// ---------------------------------------------------------------------------

router.post("/api/servers/:id/bans", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const server = await db.getServer(dbc, params.id!);
  if (!server) throw new ApiError(404, "not_found");
  if (server.owner_id !== ctx.user.id) {
    throw new ApiError(403, "forbidden", "only the owner can ban members");
  }
  const body = await readJson(ctx.request);
  const userId = body.userId;
  const reason = typeof body.reason === "string" ? body.reason.slice(0, 500) : null;
  if (typeof userId !== "string" || !userId) throw new ApiError(422, "missing_user");
  if (userId === server.owner_id) throw new ApiError(422, "cannot_ban_owner");
  // Ban = kick (remove membership) + ban row (blocks invite re-join).
  await db.addBan(dbc, server.id, userId, reason, ctx.user.id);
  await db.removeMember(dbc, server.id, userId);
  return json({ ok: true }, 201);
});

router.delete("/api/servers/:id/bans/:userId", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const server = await db.getServer(dbc, params.id!);
  if (!server) throw new ApiError(404, "not_found");
  if (server.owner_id !== ctx.user.id) {
    throw new ApiError(403, "forbidden", "only the owner can unban");
  }
  await db.removeBan(dbc, server.id, params.userId!);
  return new Response(null, { status: 204 });
});

router.get("/api/servers/:id/bans", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const server = await db.getServer(dbc, params.id!);
  if (!server) throw new ApiError(404, "not_found");
  if (server.owner_id !== ctx.user.id) {
    throw new ApiError(403, "forbidden", "only the owner can list bans");
  }
  return json(await db.listBans(dbc, server.id));
});

router.post("/api/blocks", true, async (ctx) => {
  const body = await readJson(ctx.request);
  const userId = body.userId;
  if (typeof userId !== "string" || !userId) throw new ApiError(422, "missing_user");
  if (userId === ctx.user.id) throw new ApiError(422, "cannot_block_self");
  await db.addBlock(ctx.env.LUMEN_D1, ctx.user.id, userId);
  return json({ ok: true }, 201);
});

router.delete("/api/blocks/:userId", true, async (ctx, params) => {
  await db.removeBlock(ctx.env.LUMEN_D1, ctx.user.id, params.userId!);
  return new Response(null, { status: 204 });
});

router.post("/api/reports", true, async (ctx) => {
  const dbc = ctx.env.LUMEN_D1;
  const body = await readJson(ctx.request);
  const targetType = body.targetType;
  const targetId = body.targetId;
  const reason = typeof body.reason === "string" ? body.reason.slice(0, 1000) : null;
  if (targetType !== "message" && targetType !== "user" && targetType !== "server") {
    throw new ApiError(422, "invalid_target_type");
  }
  if (typeof targetId !== "string" || !targetId) throw new ApiError(422, "missing_target");
  await db.createReport(dbc, {
    id: crypto.randomUUID(),
    reporterId: ctx.user.id,
    targetType,
    targetId,
    reason: reason ?? undefined,
  });
  return json({ ok: true }, 201);
});

router.post("/api/servers/:id/transfer", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const server = await db.getServer(dbc, params.id!);
  if (!server) throw new ApiError(404, "not_found");
  const body = await readJson(ctx.request);
  const userId = body.userId;
  if (typeof userId !== "string" || !userId) throw new ApiError(422, "missing_user");
  // Owner-only, salvo server huérfano (owner soft-deleted → cualquier miembro).
  const orphan = await db.isOwnerSoftDeleted(dbc, server.id);
  if (server.owner_id !== ctx.user.id && !orphan) {
    throw new ApiError(403, "forbidden", "only the owner can transfer the server");
  }
  if (!(await db.isMember(dbc, server.id, userId))) {
    throw new ApiError(422, "target_not_member");
  }
  if (await db.isBanned(dbc, server.id, userId)) {
    throw new ApiError(422, "target_banned");
  }
  await db.transferOwnership(dbc, server.id, userId);
  return json({ ok: true });
});

// ---------------------------------------------------------------------------
// Fase 6 — uploads (attachments) + reactions
// ---------------------------------------------------------------------------

const MAX_ATTACHMENT = 25 * 1024 * 1024; // 25 MB

/** PUT /api/uploads?filename=x — R2 attachment upload (Fase 6.4). */
router.put("/api/uploads", true, async (ctx) => {
  const filename = ctx.url.searchParams.get("filename") ?? "file";
  // Sanitize the filename for the R2 key (no path separators).
  const safe = filename.split("/").pop()?.replace(/[^A-Za-z0-9._-]/g, "_").slice(0, 120) || "file";
  const size = Number(ctx.request.headers.get("content-length") ?? 0);
  if (size > MAX_ATTACHMENT) throw new ApiError(413, "too_large", "attachment must be ≤ 25 MB");
  const blob = await ctx.request.arrayBuffer();
  if (blob.byteLength > MAX_ATTACHMENT) throw new ApiError(413, "too_large");
  const key = `attachments/${crypto.randomUUID()}/${safe}`;
  await ctx.env.LUMEN_R2.put(key, blob, {
    httpMetadata: { contentType: ctx.request.headers.get("content-type") ?? "application/octet-stream", cacheControl: "public, max-age=86400" },
  });
  return json({ url: `/api/assets/${key}` });
});

/** PUT /api/messages/:id/reactions/:emoji — toggle (Fase 6.1). */
router.put("/api/messages/:id/reactions/:emoji", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const emoji = params.emoji!.slice(0, 32);
  if (!emoji) throw new ApiError(422, "invalid_emoji");
  // Any channel member can react (message existence is not strictly checked:
  // the reaction targets a block entry; a bogus id just creates a dangling
  // row the channel purge cleans up).
  // Access: the reacting user must be able to read the channel. Resolve the
  // message's channel via its block (1 read); fall back to allowing any
  // member of the channel when the block can't be resolved.
  let channelId = "";
  const block = await dbc
    .prepare(
      `SELECT b.channel_id FROM message_blocks b, json_each(b.messages)
       WHERE json_extract(value, '$.id') = ? LIMIT 1`,
    )
    .bind(params.id!)
    .first();
  if (block) channelId = String(block.channel_id);
  if (channelId) {
    const ch = await db.getChannel(dbc, channelId);
    if (!ch || !(await canAccessChannel(dbc, ch, ctx.user.id))) {
      throw new ApiError(403, "forbidden");
    }
  }
  const existing = await dbc
    .prepare("SELECT 1 FROM reactions WHERE message_id = ? AND user_id = ? AND emoji = ?")
    .bind(params.id!, ctx.user.id, emoji)
    .first();
  if (existing) {
    await dbc
      .prepare("DELETE FROM reactions WHERE message_id = ? AND user_id = ? AND emoji = ?")
      .bind(params.id!, ctx.user.id, emoji)
      .run();
  } else {
    await dbc
      .prepare("INSERT INTO reactions (message_id, channel_id, user_id, emoji) VALUES (?, ?, ?, ?)")
      .bind(params.id!, channelId, ctx.user.id, emoji)
      .run();
  }
  return new Response(null, { status: 204 });
});

/** GET /api/channels/:id/messages/reactions?messageIds=a,b,c — agregado. */
router.get("/api/channels/:id/messages/reactions", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const channel = await db.getChannel(dbc, params.id!);
  if (!channel) throw new ApiError(404, "not_found");
  if (!(await canAccessChannel(dbc, channel, ctx.user.id))) throw new ApiError(403, "forbidden");
  const ids = (ctx.url.searchParams.get("messageIds") ?? "").split(",").filter(Boolean);
  if (ids.length === 0) return json({});
  const placeholders = ids.map(() => "?").join(",");
  const { results } = await dbc
    .prepare(
      `SELECT message_id, emoji, COUNT(*) AS n
       FROM reactions WHERE message_id IN (${placeholders})
       GROUP BY message_id, emoji`,
    )
    .bind(...ids)
    .all<{ message_id: string; emoji: string; n: number }>();
  const out: Record<string, Record<string, number>> = {};
  for (const r of results) {
    (out[r.message_id] ??= {})[r.emoji] = r.n;
  }
  return json(out);
});

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    // CORS preflight (no rate limit: the browser preflight is metadata).
    if (request.method === "OPTIONS") {
      return new Response(null, { status: 204, headers: corsHeaders(request) });
    }

    // Body size cap — fast path for requests with a known content-length
    // (chunked bodies are caught inside readJson). Asset uploads (avatar/
    // server icon, ≤ 5 MB, Fase 4) have their own size checks in the route.
    const isUpload =
      url.pathname === "/api/me/avatar" || /^\/api\/servers\/[^/]+\/icon$/.test(url.pathname);
    const contentLength = request.headers.get("content-length");
    if (!isUpload && contentLength !== null && Number(contentLength) > MAX_BODY) {
      return jsonCors({ error: "payload_too_large" }, 413, request);
    }

    // Rate limit every route except the health probe (ADR-0009).
    if (url.pathname !== "/api/health") {
      const rl = await enforceRateLimit(request, env);
      if (!rl.ok) {
        return jsonCors(
          { error: "rate_limited", retryAfter: rl.retryAfterSeconds },
          429,
          request,
        );
      }
    }

    const match = router.match(request.method, url.pathname);
    if (!match) return jsonCors({ error: "not_found" }, 404, request);

    try {
      let user: User | undefined;
      if (match.route.authed) {
        user = await requireUser(request, env);
      }
      // authed routes are guaranteed a user; non-authed handlers never read it
      const res = await match.route.handler({ request, url, env, user: user! }, match.params);
      // WS upgrades (101) carry no CORS semantics; everything else gets headers.
      if (res.status === 101) return res;
      const headers = new Headers(res.headers);
      for (const [k, v] of Object.entries(corsHeaders(request))) headers.set(k, v);
      return new Response(res.body, { status: res.status, headers });
    } catch (err) {
      if (err instanceof ApiError) {
        return jsonCors({ error: err.code }, err.status, request);
      }
      console.error("unhandled error:", err);
      return jsonCors({ error: "internal_error" }, 500, request);
    }
  },
} satisfies ExportedHandler<Env>;
