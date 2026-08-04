import type { User } from "@lumen/protocol";

import { ApiError, Router } from "./router";
import * as auth from "./auth";
import * as db from "./db";
import { getRealtimeConfig } from "./realtime";
import {
  generateInviteCode,
  validateChannelKind,
  validateChannelName,
  validateContent,
  validatePassword,
  validateServerName,
  validateUsername,
} from "./validation";

export { LumenChannelDO } from "./do/ChannelDO";

const CORS_HEADERS: Record<string, string> = {
  "access-control-allow-origin": "*",
  "access-control-allow-methods": "GET, POST, PUT, DELETE, OPTIONS",
  "access-control-allow-headers": "authorization, content-type",
};

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", ...CORS_HEADERS },
  });
}

async function readJson(request: Request): Promise<Record<string, unknown>> {
  let body: unknown;
  try {
    body = await request.json();
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
  if (!row) throw new ApiError(401, "unauthorized");
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
  return json({ token: await auth.signToken(user.id, secret), user }, 201);
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
  const ok = await auth.verifyPassword(password, `${user.password_salt}:${user.password_hash}`);
  if (!ok) throw new ApiError(401, "invalid_credentials");
  await db.updateLastSeen(ctx.env.LUMEN_D1, user.id);
  const secret = auth.getSecret(ctx.env);
  return json({ token: await auth.signToken(user.id, secret), user: db.rowToUser(user) }, 200);
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
  if (!(await db.friendshipExists(dbc, ctx.user.id, target.id))) {
    throw new ApiError(403, "not_friends", "you can only DM friends");
  }
  const existing = await db.getDmChannelBetween(dbc, ctx.user.id, target.id);
  const channel =
    existing ?? (await db.createDmChannel(dbc, crypto.randomUUID(), ctx.user.id, target.id));
  return json({ channel: db.rowToChannel(channel), otherUsername: target.username }, existing ? 200 : 201);
});

router.get("/api/dms", true, async (ctx) => {
  const rows = await db.listDmChannelsForUser(ctx.env.LUMEN_D1, ctx.user.id);
  return json(rows.map((r) => ({ channel: db.rowToChannel(r.channel), otherUsername: r.otherUsername })));
});

// messages
router.post("/api/channels/:id/messages", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const channel = await db.getChannel(dbc, params.id!);
  if (!channel) throw new ApiError(404, "not_found");
  if (!(await canAccessChannel(dbc, channel, ctx.user.id))) {
    throw new ApiError(403, "forbidden");
  }
  const body = await readJson(ctx.request);
  if (!validateContent(body.content)) {
    throw new ApiError(422, "invalid_content", "content must be 1-2000 chars");
  }
  const messageId = crypto.randomUUID();
  await db.insertMessage(dbc, {
    id: messageId,
    channelId: channel.id,
    authorId: ctx.user.id,
    content: body.content,
  });
  return json(
    {
      message: {
        id: messageId,
        channelId: channel.id,
        authorId: ctx.user.id,
        authorName: ctx.user.username,
        content: body.content,
        createdAt: new Date().toISOString(),
      },
    },
    201,
  );
});

router.get("/api/channels/:id/messages", true, async (ctx, params) => {
  const dbc = ctx.env.LUMEN_D1;
  const channel = await db.getChannel(dbc, params.id!);
  if (!channel) throw new ApiError(404, "not_found");
  if (!(await canAccessChannel(dbc, channel, ctx.user.id))) {
    throw new ApiError(403, "forbidden");
  }
  const raw = ctx.url.searchParams.get("limit");
  const parsed = raw === null ? 50 : Number.parseInt(raw, 10);
  const limit = Number.isFinite(parsed) ? Math.min(Math.max(parsed, 1), 200) : 50;
  return json(await db.listMessages(dbc, channel.id, limit));
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
// Entry
// ---------------------------------------------------------------------------

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    if (request.method === "OPTIONS") {
      return new Response(null, { status: 204, headers: CORS_HEADERS });
    }

    const match = router.match(request.method, url.pathname);
    if (!match) return json({ error: "not_found" }, 404);

    try {
      let user: User | undefined;
      if (match.route.authed) {
        user = await requireUser(request, env);
      }
      // authed routes are guaranteed a user; non-authed handlers never read it
      return await match.route.handler({ request, url, env, user: user! }, match.params);
    } catch (err) {
      if (err instanceof ApiError) {
        return json({ error: err.code }, err.status);
      }
      console.error("unhandled error:", err);
      return json({ error: "internal_error" }, 500);
    }
  },
} satisfies ExportedHandler<Env>;
