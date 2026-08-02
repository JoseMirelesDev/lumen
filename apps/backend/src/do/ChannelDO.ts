/**
 * LumenChannelDO — one Durable Object per voice channel.
 *
 * PHASE 1 OWNERSHIP: implemented by the backend task (Fase 1).
 * Mandatory design constraints (from project brief):
 *  - WebSocket Hibernation API from the first commit
 *  - process each incoming message, persist peer map to storage, hibernate immediately
 *  - no timers / no busy loops; protocol-level pings are auto-answered by the runtime
 *
 * Instances are named `lumen-${channelId}` (DO name prefix: "lumen-").
 */
export class LumenChannelDO {
  // placeholder for Fase 0 build
}
