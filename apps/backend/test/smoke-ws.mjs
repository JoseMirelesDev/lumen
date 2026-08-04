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
    ws.on("open", () => resolve(ws));
    ws.on("error", (err) => reject(err));
  });
}

function waitFor(ws, predicate, { timeoutMs = 10000, label = "message" } = {}) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      cleanup();
      reject(new Error(`timeout (${timeoutMs}ms) waiting for ${label}`));
    }, timeoutMs);
    function onMessage(raw) {
      let msg;
      try {
        msg = JSON.parse(raw.toString());
      } catch {
        return;
      }
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
      ws.off("message", onMessage);
      ws.off("close", onClose);
    }
    ws.on("message", onMessage);
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
  try {
    // ------------------------------------------------------------------
    // Part 1 — auth + REST
    // ------------------------------------------------------------------
    let res = await api("POST", "/api/auth/register", { body: alice });
    step(
      "register alice -> 201 {token,user}",
      res.status === 201 && !!res.data?.token && !!res.data?.user?.id,
      `status=${res.status}`,
    );
    const aliceId = res.data.user.id;
    const tokenA = res.data.token;

    res = await api("POST", "/api/auth/register", { body: bob });
    step("register bob -> 201", res.status === 201 && !!res.data?.token, `status=${res.status}`);
    const bobId = res.data.user.id;
    const tokenB = res.data.token;

    res = await api("POST", "/api/auth/login", { body: { username: alice.username, password: pw } });
    step(
      "login alice -> 200 {token,user}",
      res.status === 200 && !!res.data?.token && res.data.user.id === aliceId,
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

    // messages
    const msgContent = `hello from smoke ${suffix}`;
    res = await api("POST", `/api/channels/${textChannel.id}/messages`, {
      token: tokenA,
      body: { content: msgContent },
    });
    step(
      "post message -> 201 {message}",
      res.status === 201 && res.data?.message?.content === msgContent && res.data.message.authorName === alice.username,
      `status=${res.status}`,
    );
    res = await api("GET", `/api/channels/${textChannel.id}/messages`, { token: tokenB });
    step(
      "GET messages joins author username",
      res.status === 200 && res.data?.length === 1 && res.data[0].authorName === alice.username && res.data[0].content === msgContent,
      `status=${res.status}`,
    );
    res = await api("POST", `/api/channels/${textChannel.id}/messages`, { token: tokenA, body: { content: "" } });
    step("empty message -> 422", res.status === 422, `status=${res.status}`);

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
    step("non-member post message -> 403", res.status === 403, `status=${res.status}`);
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
    res = await api("POST", `/api/channels/${dmChannelId}/messages`, { token: tokenD, body: { content: "dm hi" } });
    step("post message in DM -> 201", res.status === 201, `status=${res.status}`);
    res = await api("GET", `/api/channels/${dmChannelId}/messages`, { token: tokenD });
    step("dm_user reads DM messages", res.status === 200 && Array.isArray(res.data) && res.data.length === 1, `status=${res.status}`);
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
    wsA.send(JSON.stringify({ type: "join", channelId: voiceChannel.id, userId: aliceId }));
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
    wsB.send(JSON.stringify({ type: "join", channelId: voiceChannel.id, userId: bobId }));
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
      ws.send(JSON.stringify({ type: "join", channelId: voiceChannel.id, userId: extraIds[i] }));
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
