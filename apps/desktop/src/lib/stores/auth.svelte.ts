import type { User } from "@lumen/protocol";
import { Api, ApiError } from "$lib/api";
import { getBackendUrl, setBackendUrl } from "$lib/config";

const TOKEN_KEY = "lumen.token";

class AuthStore {
  token = $state<string | null>(null);
  user = $state<User | null>(null);
  backendUrl = $state<string>(getBackendUrl());
  /** True while an auth request is in flight. */
  busy = $state(false);
  error = $state<string | null>(null);

  /** API instance bound to the current backend + token. Recreated on changes. */
  api = $derived(new Api(this.backendUrl, () => this.token));

  constructor() {
    const token = loadToken();
    if (token) {
      this.token = token;
      void this.refreshMe();
    }
  }

  async refreshMe(): Promise<void> {
    try {
      const { user } = await this.api.me();
      this.user = user;
    } catch (err) {
      // Token expired/invalid → back to the login screen.
      this.clearSession();
      if (err instanceof ApiError && err.status !== 401) this.error = err.code;
    }
  }

  async login(username: string, password: string): Promise<boolean> {
    return this.authenticate(() => this.api.login(username, password));
  }

  async register(username: string, password: string): Promise<boolean> {
    return this.authenticate(() => this.api.register(username, password));
  }

  async setBackendUrl(url: string): Promise<void> {
    const trimmed = url.trim().replace(/\/+$/, "");
    if (!trimmed) return;
    setBackendUrl(trimmed);
    this.backendUrl = trimmed;
  }

  logout(): void {
    this.clearSession();
  }

  private async authenticate(
    call: () => Promise<{ token: string; user: User }>,
  ): Promise<boolean> {
    this.busy = true;
    this.error = null;
    try {
      const { token, user } = await call();
      this.token = token;
      this.user = user;
      persistToken(token);
      return true;
    } catch (err) {
      this.error = err instanceof ApiError ? err.code : "network_error";
      return false;
    } finally {
      this.busy = false;
    }
  }

  private clearSession(): void {
    this.token = null;
    this.user = null;
    clearToken();
  }
}

function loadToken(): string | null {
  try {
    return localStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

function persistToken(token: string): void {
  try {
    localStorage.setItem(TOKEN_KEY, token);
  } catch {
    /* storage unavailable — session-only auth */
  }
}

function clearToken(): void {
  try {
    localStorage.removeItem(TOKEN_KEY);
  } catch {
    /* ignore */
  }
}

export const auth = new AuthStore();
