/**
 * Pure presence helpers for the PresenceHubDO (P3 del review): attachment
 * factory, clientId dedup (ADR-005 retransmission), and snapshot/voice-update
 * builders. No DO runtime types — plain data in, plain data out.
 */

import type { PresenceV2Status } from "@lumen/protocol";

export type PresenceAttachment = {
  userId: string;
  username: string;
  status: PresenceV2Status;
  servers: string[];
  friends: string[];
  voiceChannelId: string | null;
  voiceServerId: string | null;
  // sliding-window counters (see ws-rate-limit.ts)
  msgWindowStart: number;
  msgCount: number;
  typingWindowStart: number;
  typingCount: number;
  voiceWindowStart: number;
  voiceCount: number;
  dmWindowStart: number;
  dmCount: number;
  // last N chat clientIds — dedup on retransmission (ADR-005)
  recentClientIds: string[];
  // channels this socket receives chat RT for. Tags are immutable after
  // accept (workerd#958), so subscriptions live here instead of the tag set;
  // broadcasts iterate the server tag and filter on this list.
  subscribedChannels: string[];
};

export function createAttachment(args: {
  userId: string;
  username: string;
  servers: string[];
  friends: string[];
}): PresenceAttachment {
  return {
    userId: args.userId,
    username: args.username,
    status: "online",
    servers: args.servers,
    friends: args.friends,
    voiceChannelId: null,
    voiceServerId: null,
    msgWindowStart: 0,
    msgCount: 0,
    typingWindowStart: 0,
    typingCount: 0,
    voiceWindowStart: 0,
    voiceCount: 0,
    dmWindowStart: 0,
    dmCount: 0,
    recentClientIds: [],
    subscribedChannels: [],
  };
}

/** Dedup a retransmitted chat (client didn't get the ACK). Keeps the last
 *  `max` clientIds; a duplicate is dropped without re-broadcast. */
export function dedupClientId(
  att: PresenceAttachment,
  clientId: string,
  max = 50,
): { duplicate: boolean; recentClientIds: string[] } {
  if (att.recentClientIds.includes(clientId)) {
    return { duplicate: true, recentClientIds: att.recentClientIds };
  }
  const next = [...att.recentClientIds, clientId];
  if (next.length > max) next.splice(0, next.length - max);
  return { duplicate: false, recentClientIds: next };
}

export interface PeerLite {
  userId: string;
  username: string;
}

/** Group a server's online members by voice channel (for voice-update). */
export function groupByVoiceChannel(
  members: PeerLite[],
  voiceOf: (userId: string) => string | null,
): { channelId: string; peers: PeerLite[] }[] {
  const byVoice = new Map<string, PeerLite[]>();
  for (const m of members) {
    const channelId = voiceOf(m.userId);
    if (!channelId) continue;
    const list = byVoice.get(channelId) ?? [];
    list.push(m);
    byVoice.set(channelId, list);
  }
  return [...byVoice.entries()].map(([channelId, peers]) => ({ channelId, peers }));
}
