#!/usr/bin/env node
/**
 * STUN probe — measures STUN reachability, the public (mapped) address and
 * the NAT type from this machine, the WebRTC way.
 *
 *   node stun-probe.mjs probe            <- run on this machine
 *
 * NAT detection: two binding requests from two different local sockets to
 * the SAME STUN server. A symmetric NAT assigns a DIFFERENT mapped port per
 * destination/socket; a cone-like NAT reuses the same mapping.
 * (RFC 5780's CHANGE-REQUEST test is stricter; this is the practical proxy
 * WebRTC cares about: if mapped ports differ per socket, P2P hole punching
 * fails and TURN is mandatory.)
 */
const STUN = "stun.cloudflare.com";
const PORT = 3478;
import dgram from "node:dgram";

function binding(sock) {
  return new Promise((resolve) => {
    const tid = Buffer.from("lumen-stun-test");
    const hdr = Buffer.alloc(20);
    hdr.writeUInt16BE(0x0001, 0); // Binding request
    hdr.writeUInt16BE(0, 2); // length = 0
    hdr.writeUInt32BE(0x2112a442, 4); // magic cookie
    tid.copy(hdr, 8);
    const t = setTimeout(() => resolve(null), 4000);
    sock.once("message", (msg) => {
      clearTimeout(t);
      resolve(parse(msg));
    });
    sock.send(hdr, PORT, STUN);
  });
}

function parse(msg) {
  if (msg.length < 20) return null;
  const attrs = {};
  let off = 20;
  while (off + 4 <= msg.length) {
    const type = msg.readUInt16BE(off);
    const len = msg.readUInt16BE(off + 2);
    const v = msg.subarray(off + 4, off + 4 + len);
    if (type === 0x0001 || type === 0x0020) {
      const fam = v[1];
      let port;
      let ip;
      if (type === 0x0020) {
        // XOR-MAPPED-ADDRESS: port ^ 0x2112, IPv4 ^ magic cookie bytes
        port = (v[2] ^ 0x21) << 8 | (v[3] ^ 0x12);
        ip = `${v[4] ^ 0x21}.${v[5] ^ 0x12}.${v[6] ^ 0xa4}.${v[7] ^ 0x42}`;
      } else if (fam === 0x01) {
        port = v.readUInt16BE(2);
        ip = `${v[4]}.${v[5]}.${v[6]}.${v[7]}`;
      } else {
        port = v.readUInt16BE(2);
        ip = v.subarray(4, 20).toString("hex");
      }
      attrs[type === 0x0001 ? "mapped" : "xorMapped"] = `${ip}:${port}`;
    }
    off += 4 + ((len + 3) & ~3);
  }
  return attrs;
}

async function main() {
  const s1 = dgram.createSocket("udp4");
  const s2 = dgram.createSocket("udp4");
  const [r1, r2] = await Promise.all([binding(s1), binding(s2)]);
  s1.close(); s2.close();

  console.log(`STUN server: ${STUN}:${PORT}`);
  if (!r1 || !r2) {
    console.log("NO STUN RESPONSE — UDP to 3478 blocked?");
    process.exit(1);
  }
  const m1 = r1.mapped || r1.xorMapped;
  const m2 = r2.mapped || r2.xorMapped;
  console.log(`socket1 mapped: ${m1}`);
  console.log(`socket2 mapped: ${m2}`);

  const [ip1, p1] = m1.split(":");
  const [ip2, p2] = m2.split(":");
  if (ip1 !== ip2) {
    console.log("=> different mapped IPs per socket — extremely restrictive NAT");
  } else if (p1 !== p2) {
    console.log("=> NAT TYPE: SYMMETRIC (mapped port changes per socket)");
    console.log("   P2P hole punching will FAIL -> TURN is REQUIRED");
  } else {
    console.log("=> NAT TYPE: CONE-LIKE (same mapped port reused)");
    console.log("   P2P may work with STUN alone");
  }
}

main();
