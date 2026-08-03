import type {
  AuthResponse,
  Channel,
  FriendInfo,
  FriendshipRequest,
  RealtimeConfig,
  ServerWithChannels,
  TextMessage,
  User,
} from "@lumen/protocol";

/** API error carrying the HTTP status + machine-readable code from the worker. */
export class ApiError extends Error {
  constructor(
    public readonly status: number,
    public readonly code: string,
  ) {
    super(code);
    this.name = "ApiError";
  }
}

/**
 * Typed client for the Lumen REST API (see docs/protocol.md §4).
 * Auth is injected per-request via `getToken` so logout/session expiry is
 * handled in one place by the caller.
 */
export class Api {
  constructor(
    private readonly base: string,
    private readonly getToken: () => string | null,
  ) {}

  private async request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const headers: Record<string, string> = { "content-type": "application/json" };
    const token = this.getToken();
    if (token) headers.authorization = `Bearer ${token}`;
    const res = await fetch(`${this.base}${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    let data: unknown = null;
    try {
      data = await res.json();
    } catch {
      /* non-JSON body */
    }
    if (!res.ok) {
      const code = (data as { error?: string } | null)?.error ?? `http_${res.status}`;
      throw new ApiError(res.status, code);
    }
    return data as T;
  }

  register(username: string, password: string): Promise<AuthResponse> {
    return this.request("POST", "/api/auth/register", { username, password });
  }

  login(username: string, password: string): Promise<AuthResponse> {
    return this.request("POST", "/api/auth/login", { username, password });
  }

  me(): Promise<{ user: User }> {
    return this.request("GET", "/api/me");
  }

  createServer(name: string): Promise<{ server: ServerWithChannels["server"]; channels: Channel[] }> {
    return this.request("POST", "/api/servers", { name });
  }

  listServers(): Promise<ServerWithChannels[]> {
    return this.request("GET", "/api/servers");
  }

  joinServer(inviteCode: string): Promise<{ server: ServerWithChannels["server"]; channels: Channel[] }> {
    return this.request("POST", "/api/servers/join", { inviteCode });
  }

  getServer(serverId: string): Promise<{
    server: ServerWithChannels["server"];
    channels: Channel[];
    members: { id: string; username: string }[];
  }> {
    return this.request("GET", `/api/servers/${encodeURIComponent(serverId)}`);
  }

  createChannel(serverId: string, name: string, kind: "text" | "voice"): Promise<{ channel: Channel }> {
    return this.request("POST", `/api/servers/${encodeURIComponent(serverId)}/channels`, { name, kind });
  }

  postMessage(channelId: string, content: string): Promise<{ message: TextMessage }> {
    return this.request("POST", `/api/channels/${encodeURIComponent(channelId)}/messages`, { content });
  }

  listMessages(channelId: string, limit = 50): Promise<TextMessage[]> {
    return this.request("GET", `/api/channels/${encodeURIComponent(channelId)}/messages?limit=${limit}`);
  }

  getFriends(): Promise<{ friends: FriendInfo[]; pending: FriendshipRequest[] }> {
    return this.request("GET", "/api/friends");
  }

  getRealtimeConfig(): Promise<RealtimeConfig> {
    return this.request("GET", "/api/realtime/config");
  }
}
