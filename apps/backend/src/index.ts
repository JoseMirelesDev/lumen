/**
 * Lumen Worker entry — REST API + WebSocket upgrade to LumenChannelDO.
 *
 * PHASE 1 OWNERSHIP: this file is implemented by the backend task (Fase 1).
 * The stub below only proves the package builds during Fase 0.
 */
export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/health") {
      return Response.json({ ok: true, app: env.APP_NAME });
    }
    return Response.json({ error: "not_implemented" }, { status: 501 });
  },
} satisfies ExportedHandler<Env>;
