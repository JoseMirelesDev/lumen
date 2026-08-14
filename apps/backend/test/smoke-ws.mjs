#!/usr/bin/env node
/**
 * Lumen Fase 1 end-to-end smoke test.
 *
 * Requires `wrangler dev --local --port 8787` (apps/backend) already running.
 * Exercises the full REST surface + the WebSocket signaling flow against
 * LumenChannelDO: join/joined/peer-joined/offer/answer/ice-candidate/peer-left
 * /presence/ping-pong, and the 4-peer channel_full cap.
 *
 * Prints PASS/FAIL per step; exits non-zero if any step fails.
 */
import WebSocket from "ws";

const BASE = process.env.LUMEN_BASE ?? "http://localhost:8787";
const WS_BASE = BASE.replace(/^http/, "ws");

let failures = 0;

function step(name, ok, detail = "") {
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? ` — ${detail}` : ""}`);
  if (!ok) failures++;
}

async function api(method, path, { token, body } = {}) {
  const res = await fetch(`${BASE}${path}`, {
    method,
    headers: {
      ...(body !== undefined ? { "content-type": "application/json" } : {}),
      ...(token ? { authorization: `Bearer ${token}` } : {}),
    },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  let data = null;
  try {
    data = await res.json();
  } catch {
    /* non-JSON body */
  }
  return { status: res.status, data };
}

function openWs(path, token) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(`${WS_BASE}${path}`, {
      headers: { authorization: `Bearer ${token}` },
    });
    // Persistent per-socket message queue: the DO can push a frame (ready,
    // friend-online) before the test's waitFor attaches its listener — a
    // listener-based wait would lose it. waitFor drains the queue first.
    const q = { queue: [], listeners: [] };
    msgQueues.set(ws, q);
    ws.on("open", () => resolve(ws));
    ws.on("message", (raw) => {
      let msg;
      try {
        msg = JSON.parse(raw.toString());
      } catch {
        return;
      }
      q.queue.push(msg);
      for (const l of q.listeners) l(msg);
    });
    ws.on("error", (err) => reject(err));
  });
}

const msgQueues = new WeakMap();

function waitFor(ws, predicate, { timeoutMs = 10000, label = "message" } = {}) {
  const q = msgQueues.get(ws);
  if (!q) throw new Error("waitFor before openWs");
  return new Promise((resolve, reject) => {
    // Buffered frames first (the DO may have beaten the listener).
    const idx = q.queue.findIndex(predicate);
    if (idx !== -1) {
      const hit = q.queue[idx];
      q.queue.splice(0, idx + 1); // drain through the match
      resolve(hit);
      return;
    }
    const timer = setTimeout(() => {
      cleanup();
      reject(new Error(`timeout (${timeoutMs}ms) waiting for ${label}`));
    }, timeoutMs);
    function onMessage(msg) {
      if (predicate(msg)) {
        cleanup();
        resolve(msg);
      }
    }
    function onClose(code, reason) {
      cleanup();
      reject(new Error(`socket closed while waiting for ${label} (code=${code} reason=${reason})`));
    }
    function cleanup() {
      clearTimeout(timer);
      q.listeners = q.listeners.filter((l) => l !== onMessage);
      ws.off("close", onClose);
    }
    q.listeners.push(onMessage);
    ws.on("close", onClose);
  });
}

function waitForClose(ws, timeoutMs = 10000) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      cleanup();
      reject(new Error("timeout waiting for close"));
    }, timeoutMs);
    function onClose(code, reason) {
      cleanup();
      resolve({ code, reason });
    }
    function cleanup() {
      clearTimeout(timer);
      ws.off("close", onClose);
    }
    ws.on("close", onClose);
  });
}

async function main() {
  const suffix = Date.now().toString(36) + Math.random().toString(36).slice(2, 6);
  const pw = "password123";
  const mk = (n) => ({ username: `${n}_${suffix}`, password: pw });
  const alice = mk("alice");
  const bob = mk("bob");

  const sockets = [];
  let res;
  try {
    // ------------------------------------------------------------------
    // Part 0 — health + CORS hardening (Fase 1)
    // ------------------------------------------------------------------
    res = await api("GET", "/api/health");
    step(
      "GET /api/health -> 200 {status:ok}",
      res.status === 200 && res.data?.status === "ok" && typeof res.data?.version === "string",
      `status=${res.status}`,
    );
    const noOrigin = await fetch(`${BASE}/api/health`);
    step(
      "request without Origin -> no CORS headers (native client)",
      !noOrigin.headers.get("access-control-allow-origin"),
      String(noOrigin.headers.get("access-control-allow-origin")),
    );
    const badOrigin = await fetch(`${BASE}/api/health`, { headers: { origin: "https://evil.example" } });
    step(
      "unregistered origin -> no CORS headers",
      !badOrigin.headers.get("access-control-allow-origin"),
      String(badOrigin.headers.get("access-control-allow-origin")),
    );
    const goodOrigin = await fetch(`${BASE}/api/health`, { headers: { origin: "http://localhost:8787" } });
    step(
      "registered origin -> access-control-allow-origin echoed",
      goodOrigin.headers.get("access-control-allow-origin") === "http://localhost:8787",
      String(goodOrigin.headers.get("access-control-allow-origin")),
    );
    const preflight = await fetch(`${BASE}/api/auth/login`, {
      method: "OPTIONS",
      headers: { origin: "http://localhost:8787", "access-control-request-method": "POST" },
    });
    step("OPTIONS preflight -> 204 with CORS headers", preflight.status === 204, `status=${preflight.status}`);

    // ------------------------------------------------------------------
    // Part 1 — auth + REST
    // ------------------------------------------------------------------
    res = await api("POST", "/api/auth/register", { body: alice });
    step(
      "register alice -> 201 {token,refreshToken,user}",
      res.status === 201 && !!res.data?.token && !!res.data?.refreshToken && !!res.data?.user?.id,
      `status=${res.status}`,
    );
    const aliceId = res.data.user.id;
    const tokenA = res.data.token;

    // refresh rotation (ADR-0007): rotate -> old refresh is dead, new works
    const refresh1 = res.data.refreshToken;
    res = await api("POST", "/api/auth/refresh", { body: { refreshToken: refresh1 } });
    step(
      "POST /api/auth/refresh rotates -> 200 {token,refreshToken}",
      res.status === 200 && !!res.data?.token && !!res.data?.refreshToken,
      `status=${res.status}`,
    );
    const refresh2 = res.data.refreshToken;
    step("rotated refresh token is different", refresh2 !== refresh1);
    res = await api("POST", "/api/auth/refresh", { body: { refreshToken: refresh1 } });
    step("reused (rotated) refresh token -> 401", res.status === 401, `status=${res.status}`);
    res = await api("POST", "/api/auth/refresh", { body: { refreshToken: "bogus-token" } });
    step("unknown refresh token -> 401", res.status === 401, `status=${res.status}`);

    // logout revokes the presented refresh token
    res = await api("POST", "/api/auth/logout", { body: { refreshToken: refresh2 } });
    step("logout -> 200 {ok:true}", res.status === 200 && res.data?.ok === true, `status=${res.status}`);
    res = await api("POST", "/api/auth/refresh", { body: { refreshToken: refresh2 } });
    step("refresh after logout -> 401 (revoked)", res.status === 401, `status=${res.status}`);

    res = await api("POST", "/api/auth/register", { body: bob });
    step("register bob -> 201", res.status === 201 && !!res.data?.token, `status=${res.status}`);
    const bobId = res.data.user.id;
    const tokenB = res.data.token;

    res = await api("POST", "/api/auth/login", { body: { username: alice.username, password: pw } });
    step(
      "login alice -> 200 {token,refreshToken,user}",
      res.status === 200 && !!res.data?.token && !!res.data?.refreshToken && res.data.user.id === aliceId,
      `status=${res.status}`,
    );

    res = await api("GET", "/api/me", { token: tokenA });
    step("GET /api/me", res.status === 200 && res.data.user.id === aliceId, `status=${res.status}`);

    res = await api("POST", "/api/auth/register", { body: { username: "x!", password: "12345678" } });
    step("register invalid username -> 422", res.status === 422, `status=${res.status}`);
    res = await api("POST", "/api/auth/register", { body: { username: `dup_${suffix}`, password: "short" } });
    step("register short password -> 422", res.status === 422, `status=${res.status}`);
    res = await api("POST", "/api/auth/register", { body: alice });
    step("register duplicate username -> 409", res.status === 409, `status=${res.status}`);

    // server
    res = await api("POST", "/api/servers", { token: tokenA, body: { name: `Lumen Smoke ${suffix}` } });
    step(
      "alice creates server -> 201 {server,channels}",
      res.status === 201 && !!res.data?.server?.id && res.data.channels?.length === 2,
      `status=${res.status}`,
    );
    const serverId = res.data.server.id;
    const inviteCode = res.data.server.inviteCode;
    const voiceChannel = res.data.channels.find((c) => c.kind === "voice");
    const textChannel = res.data.channels.find((c) => c.kind === "text");
    step(
      "default channels text+voice",
      !!voiceChannel && !!textChannel,
      JSON.stringify(res.data.channels.map((c) => [c.name, c.kind])),
    );
    step(
      "invite code is 8 url-safe chars",
      typeof inviteCode === "string" && /^[A-Za-z0-9]{8}$/.test(inviteCode),
      String(inviteCode),
    );
    if (!voiceChannel || !textChannel) throw new Error("server is missing default channels");

    res = await api("POST", `/api/servers/join`, { token: tokenB, body: { inviteCode } });
    step("bob joins via invite -> 200", res.status === 200 && res.data?.server?.id === serverId, `status=${res.status}`);

    res = await api("GET", "/api/servers", { token: tokenA });
    step(
      "GET /api/servers lists member server with channels",
      Array.isArray(res.data) && res.data.some((s) => s.server.id === serverId && s.channels.length === 2),
      `status=${res.status}`,
    );

    res = await api("GET", `/api/servers/${serverId}`, { token: tokenB });
    step("member GET server -> {server,channels}", res.status === 200 && res.data?.channels?.length === 2, `status=${res.status}`);
    step(
      "server detail includes members (alice+bob)",
      Array.isArray(res.data?.members) &&
        res.data.members.length === 2 &&
        res.data.members.some((m) => m.username === alice.username) &&
        res.data.members.some((m) => m.username === bob.username),
      `members=${res.data?.members?.map((m) => m.username)}`,
    );

    // messages — REST is READ-ONLY since Fase 3 (ADR-0010): send/edit/delete
    // flow through the presence WS (Part 5 below).

    // realtime config
    res = await api("GET", "/api/realtime/config", { token: tokenA });
    step(
      "GET /api/realtime/config -> iceServers",
      res.status === 200 && Array.isArray(res.data?.iceServers) && res.data.iceServers.length > 0,
      JSON.stringify(res.data),
    );

    // error paths
    res = await api("GET", "/api/nope", { token: tokenA });
    step("unknown route -> 404 {error}", res.status === 404 && typeof res.data?.error === "string", `status=${res.status}`);
    res = await api("GET", "/api/me", {});
    step("missing token -> 401", res.status === 401, `status=${res.status}`);
    res = await api("GET", "/api/me", { token: "invalid.token.here" });
    step("invalid token -> 401", res.status === 401, `status=${res.status}`);
    res = await api("POST", `/api/servers/${serverId}/channels`, { token: tokenB, body: { name: "hax", kind: "text" } });
    step("non-owner channel create -> 403", res.status === 403, `status=${res.status}`);

    // non-member (grace never joins the server)
    const grace = mk("grace");
    res = await api("POST", "/api/auth/register", { body: grace });
    step("register grace", res.status === 201, `status=${res.status}`);
    const tokenG = res.data.token;
    res = await api("GET", `/api/servers/${serverId}`, { token: tokenG });
    step("non-member GET server -> 403", res.status === 403, `status=${res.status}`);
    res = await api("POST", `/api/channels/${textChannel.id}/messages`, { token: tokenG, body: { content: "intruder" } });
    step("REST POST message removed (read-only since Fase 3) -> 404", res.status === 404, `status=${res.status}`);
    res = await api("GET", `/api/ws/${voiceChannel.id}`, { token: tokenG });
    step("non-member WS upgrade -> 403", res.status === 403, `status=${res.status}`);

    // ------------------------------------------------------------------
    // Part 1b — DMs (friendship → create-or-get → message)
    // ------------------------------------------------------------------
    const dmUser = mk("dm_user");
    res = await api("POST", "/api/auth/register", { body: dmUser });
    step("register dm_user", res.status === 201, `status=${res.status}`);
    const tokenD = res.data.token;
    const dmUserId = res.data.user.id;

    res = await api("POST", "/api/friends/requests", { token: tokenA, body: { username: dmUser.username } });
    step("alice sends friend request to dm_user", res.status === 201, `status=${res.status}`);
    res = await api("GET", "/api/friends", { token: tokenD });
    const pendingReq = res.data.pending?.find((p) => p.id);
    step("dm_user sees pending request", res.status === 200 && (res.data.pending?.length ?? 0) === 1, `status=${res.status}`);
    res = await api("POST", `/api/friends/requests/${pendingReq.id}/accept`, { token: tokenD });
    step("dm_user accepts request", res.status === 200, `status=${res.status}`);
    res = await api("GET", "/api/friends", { token: tokenA });
    step("alice sees dm_user as friend", res.status === 200 && res.data.friends?.some((f) => f.user?.id === dmUserId), `status=${res.status}`);

    res = await api("POST", "/api/dms", { token: tokenD, body: { username: alice.username } });
    step("create DM channel -> 201", res.status === 201 && res.data.channel?.kind === "dm", `status=${res.status}`);
    const dmChannelId = res.data.channel.id;
    res = await api("POST", "/api/dms", { token: tokenD, body: { username: alice.username } });
    step("DM channel idempotent (create-or-get)", res.status === 200 && res.data.channel.id === dmChannelId, `status=${res.status}`);
    // DM messages flow through the presence WS too (Part 5).
    res = await api("GET", `/api/channels/${dmChannelId}/messages`, { token: tokenD });
    step("dm_user reads DM messages (empty)", res.status === 200 && Array.isArray(res.data) && res.data.length === 0, `status=${res.status}`);
    res = await api("GET", `/api/channels/${dmChannelId}/messages`, { token: tokenG });
    step("non-friend cannot read DM -> 403", res.status === 403, `status=${res.status}`);
    res = await api("POST", "/api/dms", { token: tokenD, body: { username: "nobody_xyz" } });
    step("DM to unknown user -> 404", res.status === 404, `status=${res.status}`);

    // ------------------------------------------------------------------
    // Part 2 — WebSocket signaling (mesh bootstrap + relays)
    // ------------------------------------------------------------------
    const wsA = await openWs(`/api/ws/${voiceChannel.id}`, tokenA);
    sockets.push(wsA);
    const joinedAP = waitFor(wsA, (m) => m.type === "joined");
    wsA.send(JSON.stringify({ type: "join", channelId: voiceChannel.id, userId: aliceId, username: alice.username }));
    const joinedA = await joinedAP;
    step(
      "A joined, peers=[]",
      joinedA.peers.length === 0 && typeof joinedA.peerId === "string",
      JSON.stringify(joinedA),
    );
    const peerA = joinedA.peerId;

    const wsB = await openWs(`/api/ws/${voiceChannel.id}`, tokenB);
    sockets.push(wsB);
    const joinedBP = waitFor(wsB, (m) => m.type === "joined");
    const peerJoinedA = waitFor(wsA, (m) => m.type === "peer-joined");
    wsB.send(JSON.stringify({ type: "join", channelId: voiceChannel.id, userId: bobId, username: bob.username }));
    const joinedB = await joinedBP;
    step(
      "B joined, peers=[A]",
      joinedB.peers.length === 1 && joinedB.peers[0].peerId === peerA,
      JSON.stringify(joinedB),
    );
    const peerB = joinedB.peerId;
    const pj = await peerJoinedA;
    step(
      "A received peer-joined B",
      pj.peer?.peerId === peerB && pj.peer?.userId === bobId,
      JSON.stringify(pj),
    );

    // relay: A -> offer -> B
    const offerAtB = waitFor(wsB, (m) => m.type === "offer");
    wsA.send(JSON.stringify({ type: "offer", to: peerB, sdp: "v=0\r\no=smokeA" }));
    const offer = await offerAtB;
    step(
      "B received offer {from:A}",
      offer.from === peerA && offer.sdp === "v=0\r\no=smokeA",
      JSON.stringify(offer),
    );

    // relay: B -> answer + ice -> A
    const answerAtA = waitFor(wsA, (m) => m.type === "answer");
    const iceAtA = waitFor(wsA, (m) => m.type === "ice-candidate");
    wsB.send(JSON.stringify({ type: "answer", to: peerA, sdp: "v=0\r\no=smokeB" }));
    wsB.send(
      JSON.stringify({
        type: "ice-candidate",
        to: peerA,
        candidate: { candidate: "candidate:1 1 udp 1 127.0.0.1 9 typ host", sdpMid: "0", sdpMLineIndex: 0 },
      }),
    );
    const answer = await answerAtA;
    const ice = await iceAtA;
    step(
      "A received answer {from:B}",
      answer.from === peerB && answer.sdp === "v=0\r\no=smokeB",
      JSON.stringify(answer),
    );
    step(
      "A received ice-candidate {from:B}",
      ice.from === peerB && ice.candidate?.candidate?.includes("udp"),
      JSON.stringify(ice),
    );

    // ping/pong
    const pong = waitFor(wsA, (m) => m.type === "pong");
    wsA.send(JSON.stringify({ type: "ping" }));
    await pong;
    step("ping -> pong", true);

    // bad message type
    const badMsg = waitFor(wsA, (m) => m.type === "error" && m.code === "bad_message");
    wsA.send(JSON.stringify({ type: "teleport" }));
    const bm = await badMsg;
    step("unknown message type -> bad_message error", bm.code === "bad_message", JSON.stringify(bm));

    // B leaves -> A sees peer-left
    const leftAtA = waitFor(wsA, (m) => m.type === "peer-left");
    wsB.close();
    const left = await leftAtA;
    step("A received peer-left B", left.peerId === peerB, JSON.stringify(left));

    // ------------------------------------------------------------------
    // Part 3 — friends
    // ------------------------------------------------------------------
    res = await api("POST", "/api/friends/requests", { token: tokenB, body: { username: alice.username } });
    step(
      "bob requests alice -> 201 outgoing",
      res.status === 201 && res.data?.request?.direction === "outgoing" && res.data.request.user.id === aliceId,
      `status=${res.status}`,
    );
    const requestId = res.data.request.id;
    res = await api("POST", "/api/friends/requests", { token: tokenB, body: { username: alice.username } });
    step("duplicate request -> 409", res.status === 409, `status=${res.status}`);
    res = await api("POST", "/api/friends/requests", { token: tokenB, body: { username: bob.username } });
    step("self request -> 422", res.status === 422, `status=${res.status}`);

    res = await api("GET", "/api/friends", { token: tokenA });
    const incoming = res.data?.pending?.find((r) => r.id === requestId);
    step(
      "alice sees incoming pending request",
      res.status === 200 && incoming?.direction === "incoming" && incoming.user.id === bobId,
      `status=${res.status}`,
    );

    res = await api("POST", `/api/friends/requests/${requestId}/accept`, { token: tokenA });
    step("alice accepts -> 200 {friend}", res.status === 200 && res.data?.friend?.id === bobId, `status=${res.status}`);

    res = await api("GET", "/api/friends", { token: tokenA });
    const friendA = res.data?.friends?.find((f) => f.user.id === bobId);
    step(
      "alice friends includes bob (sharedServers>=1)",
      !!friendA && friendA.sharedServers >= 1,
      JSON.stringify(friendA),
    );
    res = await api("GET", "/api/friends", { token: tokenB });
    const friendB = res.data?.friends?.find((f) => f.user.id === aliceId);
    step("bob friends includes alice", !!friendB, JSON.stringify(friendB));
    res = await api("GET", "/api/friends", { token: tokenA });
    step("alice pending empty after accept", Array.isArray(res.data?.pending) && res.data.pending.length === 0, JSON.stringify(res.data?.pending));

    // ------------------------------------------------------------------
    // Part 4 — channel capacity (max 4 peers)
    // ------------------------------------------------------------------
    const extra = ["charlie", "dave", "erin", "frank"].map((n) => mk(n));
    const extraTokens = [];
    const extraIds = [];
    for (const u of extra) {
      res = await api("POST", "/api/auth/register", { body: u });
      if (res.status !== 201) throw new Error(`register ${u.username} failed: ${res.status}`);
      extraTokens.push(res.data.token);
      extraIds.push(res.data.user.id);
      res = await api("POST", `/api/servers/join`, { token: res.data.token, body: { inviteCode } });
      if (res.status !== 200) throw new Error(`join ${u.username} failed: ${res.status}`);
    }

    // C, D, E join (A is still connected; B left) -> 4 peers present
    for (let i = 0; i < 3; i++) {
      const ws = await openWs(`/api/ws/${voiceChannel.id}`, extraTokens[i]);
      sockets.push(ws);
      const joined = waitFor(ws, (m) => m.type === "joined");
      ws.send(JSON.stringify({ type: "join", channelId: voiceChannel.id, userId: extraIds[i], username: "extra" }));
      await joined;
    }
    step("3 more peers joined (4 total)", true);

    // presence broadcast to the extra peers (sockets[2] = charlie, first extra peer)
    const presenceAtC = waitFor(sockets[2], (m) => m.type === "presence");
    wsA.send(JSON.stringify({ type: "presence", status: "idle" }));
    const pres = await presenceAtC;
    step(
      "presence broadcast to peers",
      pres.userId === aliceId && pres.status === "idle",
      JSON.stringify(pres),
    );

    // 5th peer -> channel_full error then close
    const wsF = await openWs(`/api/ws/${voiceChannel.id}`, extraTokens[3]);
    sockets.push(wsF);
    const fullErr = waitFor(wsF, (m) => m.type === "error" && m.code === "channel_full");
    const fullClose = waitForClose(wsF);
    wsF.send(JSON.stringify({ type: "join", channelId: voiceChannel.id, userId: extraIds[3] }));
    const err = await fullErr;
    const closeInfo = await fullClose;
    step(
      "5th peer gets channel_full error + close",
      err.code === "channel_full" && typeof closeInfo.code === "number",
      JSON.stringify(err),
    );
    // ------------------------------------------------------------------
    // Part 5 — Fase 3: presence WS + real-time chat (ADR-003/004/005/0010)
    // ------------------------------------------------------------------
    // NOTE: alice and bob are already friends (Part 3) and members of the
    // same server. Messages are sent ONLY via the presence WS now.
    const wsP = await openWs("/api/presence", tokenA);
    sockets.push(wsP);
    const readyA = await waitFor(wsP, (m) => m.type === "ready");
    step(
      "presence ready: server presence with self online",
      readyA.servers.some((s) => s.serverId === serverId && s.onlineMembers.some((m) => m.userId === aliceId)),
      JSON.stringify(readyA.servers),
    );
    step("presence ready: no friends online yet", Array.isArray(readyA.onlineFriends) && readyA.onlineFriends.length === 0, JSON.stringify(readyA.onlineFriends));

    // bob connects → alice sees friend-online + member-online; bob's ready
    // shows alice as an online friend.
    const wsQ = await openWs("/api/presence", tokenB);
    sockets.push(wsQ);
    const friendOnlineAtA = waitFor(wsP, (m) => m.type === "friend-online" && m.userId === bobId);
    const memberOnlineAtA = waitFor(wsP, (m) => m.type === "member-online" && m.userId === bobId);
    const readyB = await waitFor(wsQ, (m) => m.type === "ready");
    const friendOnline = await friendOnlineAtA;
    const memberOnline = await memberOnlineAtA;
    step(
      "alice sees bob friend-online",
      friendOnline.userId === bobId && friendOnline.username === bob.username,
      JSON.stringify(friendOnline),
    );
    step(
      "alice sees bob member-online in the server",
      memberOnline.serverId === serverId && memberOnline.userId === bobId,
      JSON.stringify(memberOnline),
    );
    step(
      "bob's ready lists alice as online friend",
      readyB.onlineFriends.some((f) => f.userId === aliceId),
      JSON.stringify(readyB.onlineFriends),
    );
    step(
      "bob's ready lists alice as online member",
      readyB.servers.some((s) => s.serverId === serverId && s.onlineMembers.some((m) => m.userId === aliceId)),
      JSON.stringify(readyB.servers),
    );

    // subscribe both to the text channel — wait for the acks (the chat
    // broadcast filters by subscription; without the ack a chat sent right
    // after subscribe can race the wsQ subscribe and be dropped)
    const subAckA = waitFor(wsP, (m) => m.type === "subscribe-ack" && m.channelId === textChannel.id);
    const subAckB = waitFor(wsQ, (m) => m.type === "subscribe-ack" && m.channelId === textChannel.id);
    wsP.send(JSON.stringify({ type: "subscribe", channelId: textChannel.id }));
    wsQ.send(JSON.stringify({ type: "subscribe", channelId: textChannel.id }));
    const subAckAmsg = await subAckA;
    const subAckBmsg = await subAckB;
    step(
      "subscribe-ack on both sockets",
      subAckAmsg.channelId === textChannel.id && subAckBmsg.channelId === textChannel.id,
      JSON.stringify({ subAckAmsg, subAckBmsg }),
    );

    // chat: alice → ack + bob receives
    const chatAtB = waitFor(wsQ, (m) => m.type === "chat" && m.channelId === textChannel.id);
    const ackA = waitFor(wsP, (m) => m.type === "chat-ack");
    wsP.send(JSON.stringify({ type: "chat", channelId: textChannel.id, serverId, content: `hello rt ${suffix}`, clientId: "c1" }));
    const ack = await ackA;
    const chat = await chatAtB;
    step(
      "chat RT: sender gets chat-ack with messageId",
      ack.clientId === "c1" && typeof ack.messageId === "string" && !!ack.createdAt,
      JSON.stringify(ack),
    );
    step(
      "chat RT: subscriber receives the message",
      chat.message.content === `hello rt ${suffix}` && chat.message.authorId === aliceId,
      JSON.stringify(chat),
    );
    const rtMessageId = ack.messageId;

    // chat-edit → ack + chat-edited broadcast
    const editedAtB = waitFor(wsQ, (m) => m.type === "chat-edited" && m.channelId === textChannel.id);
    const editAckA = waitFor(wsP, (m) => m.type === "chat-edit-ack");
    wsP.send(JSON.stringify({ type: "chat-edit", channelId: textChannel.id, serverId, messageId: rtMessageId, content: "edited rt!", clientId: "e1" }));
    const editAck = await editAckA;
    const edited = await editedAtB;
    step(
      "chat-edit: ack + chat-edited broadcast",
      editAck.messageId === rtMessageId && edited.message.id === rtMessageId && edited.message.content === "edited rt!" && !!edited.message.editedAt,
      JSON.stringify({ editAck, edited }),
    );
    // non-author edit rejected
    const editErr = waitFor(wsQ, (m) => m.type === "error" && m.code === "bad_message");
    wsQ.send(JSON.stringify({ type: "chat-edit", channelId: textChannel.id, serverId, messageId: rtMessageId, content: "hax", clientId: "e2" }));
    const eErr = await editErr;
    step("non-author chat-edit -> bad_message", eErr.code === "bad_message", JSON.stringify(eErr));

    // chat-delete → ack + chat-deleted broadcast
    const deletedAtB = waitFor(wsQ, (m) => m.type === "chat-deleted" && m.channelId === textChannel.id);
    const delAckA = waitFor(wsP, (m) => m.type === "chat-delete-ack");
    wsP.send(JSON.stringify({ type: "chat-delete", channelId: textChannel.id, serverId, messageId: rtMessageId, clientId: "d1" }));
    const delAck = await delAckA;
    const deleted = await deletedAtB;
    step(
      "chat-delete: ack + chat-deleted broadcast",
      delAck.messageId === rtMessageId && deleted.messageId === rtMessageId,
      JSON.stringify({ delAck, deleted }),
    );

    // GET messages reflects the buffer (deleted entry stays as placeholder)
    res = await api("GET", `/api/channels/${textChannel.id}/messages`, { token: tokenB });
    step(
      "GET messages returns buffered message with deletedAt",
      res.status === 200 && res.data.length === 1 && res.data[0].id === rtMessageId && !!res.data[0].deletedAt,
      JSON.stringify(res.data),
    );

    // voice occupancy without entering: alice voice-joins → bob gets voice-update
    const voiceUpdB = waitFor(wsQ, (m) => m.type === "voice-update" && m.channelId === voiceChannel.id);
    wsP.send(JSON.stringify({ type: "voice-join", channelId: voiceChannel.id, serverId }));
    const voiceUpd = await voiceUpdB;
    step(
      "voice-join → voice-update to server members (occupancy without entering)",
      voiceUpd.serverId === serverId && voiceUpd.peers.some((p) => p.userId === aliceId),
      JSON.stringify(voiceUpd),
    );
    wsP.send(JSON.stringify({ type: "voice-leave" }));

    // DM chat via presence WS (serverId = "" for DM channels)
    wsP.send(JSON.stringify({ type: "chat", channelId: dmChannelId, serverId: "", content: "dm via ws", clientId: "dm1" }));
    const dmAck = await waitFor(wsP, (m) => m.type === "chat-ack" && m.clientId === "dm1");
    step("DM chat via presence WS -> ack", !!dmAck.messageId, JSON.stringify(dmAck));
    res = await api("GET", `/api/channels/${dmChannelId}/messages`, { token: tokenD });
    step(
      "DM message readable via GET",
      res.status === 200 && res.data.length === 1 && res.data[0].content === "dm via ws",
      JSON.stringify(res.data),
    );

    // typing broadcast (subscribers only)
    const typingAtB = waitFor(wsQ, (m) => m.type === "typing" && m.channelId === textChannel.id);
    wsP.send(JSON.stringify({ type: "typing", channelId: textChannel.id, serverId }));
    const typing = await typingAtB;
    step("typing broadcast to subscribers", typing.userId === aliceId, JSON.stringify(typing));

    // ping/pong on presence socket
    const pongP = waitFor(wsP, (m) => m.type === "pong");
    wsP.send(JSON.stringify({ type: "ping" }));
    await pongP;
    step("presence ping -> pong", true);

    // rate limit chat: 10 msgs/10s (rate limiter disabled in dev via
    // LUMEN_RATE_LIMIT_DISABLED, so this exercises the pure module in unit
    // tests instead — see test/do-lib.test.ts).

    // flush + pagination: 101 messages → 2 blocks (50 each) + 1 in buffer
    const flushCount = 101;
    for (let i = 0; i < flushCount; i++) {
      wsP.send(JSON.stringify({ type: "chat", channelId: textChannel.id, serverId, content: `bulk ${i}`, clientId: `b${i}` }));
      if (i % 10 === 0) await new Promise((r) => setTimeout(r, 10)); // distinct ms across blocks
    }
    res = await api("GET", `/api/channels/${textChannel.id}/messages`, { token: tokenA });
    step(
      "newest page = newest block + buffer (52 msgs)",
      res.status === 200 && res.data.length === 52 && res.data[51].content === `bulk 100`,
      `len=${res.data.length}`,
    );
    // scroll up with the oldest loaded message's composite cursor (P5)
    const oldest = res.data[0];
    const cursor = `${oldest.createdAt},${oldest.id}`;
    res = await api("GET", `/api/channels/${textChannel.id}/messages?before=${encodeURIComponent(cursor)}`, { token: tokenA });
    step(
      "scroll up loads the previous block (50 msgs, 1 D1 read)",
      res.status === 200 && res.data.length === 50 && res.data[0].deletedAt && res.data[1].content === "bulk 0",
      `len=${res.data.length} first=${res.data[0]?.content}`,
    );

    // ws close → bob sees member-offline + friend-offline; last_seen updated
    const memberOffAtB = waitFor(wsQ, (m) => m.type === "member-offline" && m.userId === aliceId);
    const friendOffAtB = waitFor(wsQ, (m) => m.type === "friend-offline" && m.userId === aliceId);
    wsP.close();
    await memberOffAtB;
    await friendOffAtB;
    step("alice closes → bob sees member-offline + friend-offline", true);

    // ------------------------------------------------------------------
    // Part 6 — Fase 2 CRUD (servers, channels, profile, friends, DMs)
    // ------------------------------------------------------------------
    const renamed = `Lumen Smoke R ${suffix}`;
    res = await api("PATCH", `/api/servers/${serverId}`, { token: tokenA, body: { name: renamed } });
    step(
      "owner PATCH server name -> 200",
      res.status === 200 && res.data?.server?.name === renamed,
      `status=${res.status}`,
    );
    res = await api("PATCH", `/api/servers/${serverId}`, { token: tokenB, body: { name: "hax" } });
    step("member PATCH server -> 403", res.status === 403, `status=${res.status}`);

    res = await api("POST", `/api/servers/${serverId}/invite`, { token: tokenA });
    step(
      "owner regenerates invite -> 200 new code",
      res.status === 200 && typeof res.data?.inviteCode === "string" && res.data.inviteCode !== inviteCode,
      `status=${res.status}`,
    );
    const inviteCode2 = res.data.inviteCode;

    // kick bob (owner), then bob is no longer a member
    res = await api("DELETE", `/api/servers/${serverId}/members/${bobId}`, { token: tokenA });
    step("owner kicks bob -> 204", res.status === 204, `status=${res.status}`);
    res = await api("GET", `/api/servers/${serverId}`, { token: tokenB });
    step("kicked member GET server -> 403", res.status === 403, `status=${res.status}`);
    res = await api("POST", `/api/servers/join`, { token: tokenB, body: { inviteCode: inviteCode2 } });
    step("kicked member re-joins via new invite -> 200", res.status === 200, `status=${res.status}`);

    res = await api("DELETE", `/api/servers/${serverId}`, { token: tokenA });
    step("delete server without ?confirm=true -> 400", res.status === 400, `status=${res.status}`);

    // channel create -> patch (name/topic) -> delete
    res = await api("POST", `/api/servers/${serverId}/channels`, { token: tokenA, body: { name: "tmp", kind: "text" } });
    step("owner creates channel -> 201", res.status === 201 && !!res.data?.channel?.id, `status=${res.status}`);
    const tmpChannelId = res.data.channel.id;
    res = await api("PATCH", `/api/channels/${tmpChannelId}`, { token: tokenA, body: { name: "tmp2", topic: "topic here" } });
    step(
      "owner PATCH channel name+topic -> 200",
      res.status === 200 && res.data?.channel?.name === "tmp2" && res.data.channel.topic === "topic here",
      `status=${res.status}`,
    );
    res = await api("PATCH", `/api/channels/${tmpChannelId}`, { token: tokenB, body: { name: "hax" } });
    step("member PATCH channel -> 403", res.status === 403, `status=${res.status}`);
    res = await api("DELETE", `/api/channels/${tmpChannelId}`, { token: tokenB });
    step("member DELETE channel -> 403", res.status === 403, `status=${res.status}`);
    res = await api("DELETE", `/api/channels/${tmpChannelId}`, { token: tokenA });
    step("owner DELETE channel -> 204", res.status === 204, `status=${res.status}`);

    // message edit/delete — via the presence WS only since Fase 3 (ADR-0010);
    // covered in Part 5 (chat-edit / chat-delete with ACK + broadcasts).

    // profile: password change
    const newPw = "newpassword456";
    res = await api("PUT", "/api/me/password", { token: tokenA, body: { currentPassword: pw, newPassword: newPw } });
    step("change password -> 204", res.status === 204, `status=${res.status}`);
    res = await api("POST", "/api/auth/login", { body: { username: alice.username, password: pw } });
    step("login with old password -> 401", res.status === 401, `status=${res.status}`);
    res = await api("POST", "/api/auth/login", { body: { username: alice.username, password: newPw } });
    step("login with new password -> 200", res.status === 200 && !!res.data?.token, `status=${res.status}`);

    // profile: username conflict
    const nameHog = mk("namehog");
    res = await api("POST", "/api/auth/register", { body: nameHog });
    step("register namehog", res.status === 201, `status=${res.status}`);
    res = await api("PATCH", "/api/me", { token: tokenA, body: { username: nameHog.username } });
    step("username conflict -> 409", res.status === 409, `status=${res.status}`);
    const aliceRenamed = `alice_ren_${suffix}`;
    res = await api("PATCH", "/api/me", { token: tokenA, body: { username: aliceRenamed } });
    step("rename alice -> 200", res.status === 200 && res.data?.user?.username === aliceRenamed, `status=${res.status}`);

    // friends: remove dm_user, DM soft delete per-user
    res = await api("DELETE", `/api/friends/${dmUserId}`, { token: tokenA });
    step("remove friend -> 204", res.status === 204, `status=${res.status}`);
    res = await api("GET", "/api/friends", { token: tokenA });
    step(
      "removed friend no longer listed",
      res.status === 200 && !res.data.friends?.some((f) => f.user?.id === dmUserId),
      `status=${res.status}`,
    );
    res = await api("DELETE", `/api/dms/${dmChannelId}`, { token: tokenA });
    step("delete DM for self -> 204", res.status === 204, `status=${res.status}`);
    res = await api("GET", "/api/dms", { token: tokenA });
    step("DM gone from alice's list", res.status === 200 && !res.data.some((d) => d.channel?.id === dmChannelId), `status=${res.status}`);
    res = await api("GET", "/api/dms", { token: tokenD });
    step("DM still visible to the other participant", res.status === 200 && res.data.some((d) => d.channel?.id === dmChannelId), `status=${res.status}`);

    // profile: soft-delete account
    const victim = mk("victim");
    res = await api("POST", "/api/auth/register", { body: victim });
    step("register victim", res.status === 201, `status=${res.status}`);
    const victimToken = res.data.token;
    res = await api("DELETE", "/api/me", { token: victimToken });
    step("DELETE /api/me -> 204", res.status === 204, `status=${res.status}`);
    res = await api("POST", "/api/auth/login", { body: { username: victim.username, password: pw } });
    step("login of soft-deleted user -> 401", res.status === 401, `status=${res.status}`);
    res = await api("GET", "/api/me", { token: victimToken });
    step("access token of soft-deleted user -> 401", res.status === 401, `status=${res.status}`);

    // owner leave forbidden
    res = await api("POST", `/api/servers/${serverId}/leave`, { token: tokenA });
    step("owner cannot leave -> 403", res.status === 403, `status=${res.status}`);

    // ------------------------------------------------------------------
    // Part 7 — Fase 4: OAuth flow shape + R2 assets
    // ------------------------------------------------------------------
    res = await fetch(`${BASE}/api/oauth/google`, { redirect: "manual" });
    step(
      "GET /api/oauth/google -> 302 to provider with state",
      res.status === 302 && res.headers.get("location")?.includes("accounts.google.com") && res.headers.get("location")?.includes("state="),
      `status=${res.status} loc=${res.headers.get("location")?.slice(0, 60)}`,
    );
    res = await fetch(`${BASE}/api/oauth/github`, { redirect: "manual" });
    step(
      "GET /api/oauth/github -> 302 to provider",
      res.status === 302 && res.headers.get("location")?.includes("github.com"),
      `status=${res.status}`,
    );
    res = await api("GET", "/api/oauth/unknown");
    step("unknown OAuth provider -> 404", res.status === 404, `status=${res.status}`);
    res = await api("GET", "/api/oauth/google/callback");
    step("callback without params -> 400", res.status === 400, `status=${res.status}`);
    res = await api("GET", "/api/oauth/google/callback?code=x&state=bogus");
    step("callback with invalid state -> 400 invalid_state", res.status === 400, `status=${res.status}`);
    // Reused state → 400 (consumed on first use). We cannot pre-insert a
    // state here, but the invalid-state path above covers the anti-CSRF.

    // avatar upload → R2 → public GET
    const png = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4]);
    const up = await fetch(`${BASE}/api/me/avatar`, {
      method: "PUT",
      headers: { authorization: `Bearer ${tokenA}`, "content-type": "image/png" },
      body: png,
    });
    let upData = null;
    try { upData = await up.json(); } catch {}
    step(
      "PUT /api/me/avatar -> 200 {url}",
      up.status === 200 && typeof upData?.url === "string" && upData.url.startsWith("/api/assets/avatars/"),
      `status=${up.status} ${JSON.stringify(upData)}`,
    );
    const assetPath = upData?.url;
    const asset = await fetch(`${BASE}${assetPath}`);
    step(
      "GET /api/assets/avatars/<id>.png -> 200 image/png",
      asset.status === 200 && asset.headers.get("content-type")?.includes("image/png"),
      `status=${asset.status} ct=${asset.headers.get("content-type")}`,
    );
    // avatar > 5 MB → 413 (route check; body limit exempts uploads)
    const big = new Uint8Array(6 * 1024 * 1024);
    const bigUp = await fetch(`${BASE}/api/me/avatar`, {
      method: "PUT",
      headers: { authorization: `Bearer ${tokenA}` },
      body: big,
    });
    step("avatar > 5 MB -> 413", bigUp.status === 413, `status=${bigUp.status}`);

    // server icon (owner) + permission check
    const icon = await fetch(`${BASE}/api/servers/${serverId}/icon`, {
      method: "PUT",
      headers: { authorization: `Bearer ${tokenA}`, "content-type": "image/png" },
      body: png,
    });
    step("owner PUT server icon -> 200", icon.status === 200, `status=${icon.status}`);
    const iconB = await fetch(`${BASE}/api/servers/${serverId}/icon`, {
      method: "PUT",
      headers: { authorization: `Bearer ${tokenB}` },
      body: png,
    });
    step("member PUT server icon -> 403", iconB.status === 403, `status=${iconB.status}`);

    // ------------------------------------------------------------------
    // Part 8 — Fase 5: moderación (bans, blocks, reports, transfer)
    // ------------------------------------------------------------------
    const victim2 = mk("victim2b");
    res = await api("POST", "/api/auth/register", { body: victim2 });
    step("register victim2", res.status === 201, `status=${res.status}`);
    const tokenV = res.data.token;
    const victimId = res.data.user.id;
    res = await api("POST", "/api/servers/join", { token: tokenV, body: { inviteCode: inviteCode2 } });
    step("victim2 joins via invite -> 200", res.status === 200, `status=${res.status}`);


    // member cannot ban
    res = await api("POST", `/api/servers/${serverId}/bans`, { token: tokenB, body: { userId: victimId } });
    step("member ban -> 403", res.status === 403, `status=${res.status}`);
    // owner bans → kicked + join blocked
    res = await api("POST", `/api/servers/${serverId}/bans`, { token: tokenA, body: { userId: victimId, reason: "spam" } });
    step("owner bans victim2 -> 201", res.status === 201, `status=${res.status}`);
    res = await api("GET", `/api/servers/${serverId}`, { token: tokenV });
    step("banned user loses membership (GET -> 403)", res.status === 403, `status=${res.status}`);
    res = await api("POST", "/api/servers/join", { token: tokenV, body: { inviteCode: inviteCode2 } });
    step("banned user join by invite -> 403", res.status === 403, `status=${res.status}`);
    res = await api("GET", `/api/servers/${serverId}/bans`, { token: tokenA });
    step(
      "owner lists bans (contains victim2)",
      res.status === 200 && res.data.some((b) => b.user_id === victimId),
      JSON.stringify(res.data),
    );
    res = await api("DELETE", `/api/servers/${serverId}/bans/${victimId}`, { token: tokenA });
    step("owner unbans -> 204", res.status === 204, `status=${res.status}`);
    res = await api("POST", "/api/servers/join", { token: tokenV, body: { inviteCode: inviteCode2 } });
    step("unbanned user re-joins -> 200", res.status === 200, `status=${res.status}`);

    // transfer ownership: alice → bob; bob manages; alice no longer does
    res = await api("POST", `/api/servers/${serverId}/transfer`, { token: tokenA, body: { userId: bobId } });
    step("owner transfers server to bob -> 200", res.status === 200, `status=${res.status}`);
    res = await api("PATCH", `/api/servers/${serverId}`, { token: tokenB, body: { name: `${renamed} T` } });
    step("new owner edits server -> 200", res.status === 200 && res.data?.server?.ownerId === bobId, `status=${res.status}`);
    res = await api("PATCH", `/api/servers/${serverId}`, { token: tokenA, body: { name: "nope" } });
    step("old owner edit -> 403", res.status === 403, `status=${res.status}`);
    res = await api("POST", `/api/servers/${serverId}/transfer`, { token: tokenA, body: { userId: aliceId } });
    step("old owner transfer back -> 403", res.status === 403, `status=${res.status}`);
    res = await api("POST", `/api/servers/${serverId}/transfer`, { token: tokenB, body: { userId: aliceId } });
    step("new owner transfers back -> 200", res.status === 200, `status=${res.status}`);

    // blocks: alice blocks dm_user → dm_user can't friend-request alice
    res = await api("POST", "/api/blocks", { token: tokenA, body: { userId: dmUserId } });
    step("alice blocks dm_user -> 201", res.status === 201, `status=${res.status}`);
    res = await api("POST", "/api/friends/requests", { token: tokenD, body: { username: aliceRenamed } });
    step("blocked user friend request -> 403", res.status === 403, `status=${res.status}`);
    res = await api("DELETE", `/api/blocks/${dmUserId}`, { token: tokenA });
    step("alice unblocks dm_user -> 204", res.status === 204, `status=${res.status}`);
    res = await api("POST", "/api/friends/requests", { token: tokenD, body: { username: aliceRenamed } });
    step("friend request works after unblock -> 201", res.status === 201, `status=${res.status}`);

    // reports
    res = await api("POST", "/api/reports", { token: tokenA, body: { targetType: "message", targetId: rtMessageId, reason: "test report" } });
    step("report message -> 201", res.status === 201, `status=${res.status}`);
    res = await api("POST", "/api/reports", { token: tokenA, body: { targetType: "bogus", targetId: "x" } });
    step("report invalid target_type -> 422", res.status === 422, `status=${res.status}`);

    // ------------------------------------------------------------------
    // Part 9 — Fase 6: replies (6.2), attachments (6.4), reactions (6.1)
    // ------------------------------------------------------------------
    // alice's presence socket was closed in Part 5 — reconnect + subscribe.
    const wsR = await openWs("/api/presence", tokenA);
    sockets.push(wsR);
    await waitFor(wsR, (m) => m.type === "ready");
    wsR.send(JSON.stringify({ type: "subscribe", channelId: textChannel.id }));
    // replies: chat with replyTo via WS → stored in the buffer → GET echoes it
    const replyAck = waitFor(wsQ, (m) => m.type === "chat-ack");
    wsQ.send(JSON.stringify({ type: "chat", channelId: textChannel.id, serverId, content: "replying", clientId: "r1", replyTo: rtMessageId }));
    const rAck = await replyAck;
    step("chat with replyTo -> ack", !!rAck.messageId, JSON.stringify(rAck));
    res = await api("GET", `/api/channels/${textChannel.id}/messages`, { token: tokenA });
    const replyMsg = res.data.find((m) => m.content === "replying");
    step(
      "GET returns replyTo on the replied message",
      res.status === 200 && replyMsg?.replyTo === rtMessageId,
      `replyTo=${replyMsg?.replyTo}`,
    );

    // attachments: upload → R2 url → chat with attachmentUrl → GET echoes it
    const attUp = await fetch(`${BASE}/api/uploads?filename=test.txt`, {
      method: "PUT",
      headers: { authorization: `Bearer ${tokenA}`, "content-type": "text/plain" },
      body: "hello attachment",
    });
    let attData = null;
    try { attData = await attUp.json(); } catch {}
    step(
      "PUT /api/uploads -> 200 {url}",
      attUp.status === 200 && typeof attData?.url === "string" && attData.url.includes("/api/assets/attachments/"),
      `status=${attUp.status} ${JSON.stringify(attData)}`,
    );
    const attAsset = await fetch(`${BASE}${attData.url}`);
    step("attachment GET -> 200", attAsset.status === 200 && (await attAsset.text()) === "hello attachment", `status=${attAsset.status}`);
    const bigAtt = await fetch(`${BASE}/api/uploads?filename=big.bin`, {
      method: "PUT",
      headers: { authorization: `Bearer ${tokenA}` },
      body: new Uint8Array(26 * 1024 * 1024),
    });
    step("attachment > 25 MB -> 413", bigAtt.status === 413, `status=${bigAtt.status}`);

    const attAck = waitFor(wsQ, (m) => m.type === "chat-ack");
    wsQ.send(JSON.stringify({ type: "chat", channelId: textChannel.id, serverId, content: "with attachment", clientId: "r2", attachmentUrl: attData.url }));
    await attAck;
    res = await api("GET", `/api/channels/${textChannel.id}/messages`, { token: tokenA });
    const attMsg = res.data.find((m) => m.content === "with attachment");
    step(
      "GET returns attachmentUrl on the message",
      res.status === 200 && attMsg?.attachmentUrl === attData.url,
      `url=${attMsg?.attachmentUrl}`,
    );

    // reactions: WS toggle + broadcast + REST toggle + aggregated GET
    const reactAtA = waitFor(wsR, (m) => m.type === "reaction" && m.messageId === rtMessageId);
    wsQ.send(JSON.stringify({ type: "reaction-toggle", channelId: textChannel.id, serverId, messageId: rtMessageId, emoji: "❤️" }));
    const react = await reactAtA;
    step(
      "reaction-toggle -> broadcast {added:true}",
      react.added === true && react.emoji === "❤️" && react.userId === bobId,
      JSON.stringify(react),
    );
    // REST toggle off + aggregated counts
    res = await api("PUT", `/api/messages/${rtMessageId}/reactions/${encodeURIComponent("❤️")}`, { token: tokenA });
    step("REST reaction toggle (alice adds) -> 204", res.status === 204, `status=${res.status}`);
    res = await api("GET", `/api/channels/${textChannel.id}/messages/reactions?messageIds=${rtMessageId}`, { token: tokenA });
    step(
      "aggregated reactions: bob(WS) + alice(REST) = 2",
      res.status === 200 && res.data[rtMessageId]?.["❤️"] === 2,
      JSON.stringify(res.data),
    );
    res = await api("PUT", `/api/messages/${rtMessageId}/reactions/${encodeURIComponent("❤️")}`, { token: tokenA });
    step("REST reaction toggle (alice removes) -> 204", res.status === 204, `status=${res.status}`);
    res = await api("GET", `/api/channels/${textChannel.id}/messages/reactions?messageIds=${rtMessageId}`, { token: tokenA });
    step(
      "aggregated reactions: alice removed hers, bob's remains = 1",
      res.status === 200 && res.data[rtMessageId]?.["❤️"] === 1,
      JSON.stringify(res.data),
    );
  } catch (err) {
    failures++;
    console.error("SMOKE ERROR:", err.message ?? err);
  } finally {
    for (const ws of sockets) {
      try {
        ws.terminate();
      } catch {
        /* already gone */
      }
    }
  }

  console.log("");
  console.log(failures === 0 ? "ALL STEPS PASSED" : `${failures} STEP(S) FAILED`);
  process.exit(failures === 0 ? 0 : 1);
}

main();
