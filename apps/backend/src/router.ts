import type { User } from "@lumen/protocol";

/** Error carrying an HTTP status + machine-readable code for JSON responses. */
export class ApiError extends Error {
  constructor(
    public readonly status: number,
    public readonly code: string,
    message?: string,
  ) {
    super(message ?? code);
    this.name = "ApiError";
  }
}

export interface RouteContext {
  request: Request;
  url: URL;
  env: Env;
  /** Present on authed routes (router resolves it before calling the handler). */
  user: User;
}

export type RouteHandler = (
  ctx: RouteContext,
  params: Record<string, string>,
) => Promise<Response> | Response;

interface RouteDef {
  method: string;
  pattern: RegExp;
  paramNames: string[];
  authed: boolean;
  handler: RouteHandler;
}

function compilePath(template: string): { pattern: RegExp; paramNames: string[] } {
  const paramNames: string[] = [];
  const source = template.replace(/:[A-Za-z]+/g, (m) => {
    paramNames.push(m.slice(1));
    return "([^/]+)";
  });
  return { pattern: new RegExp(`^${source}$`), paramNames };
}

/**
 * Minimal path router: method + `:param` template, ordered first-match.
 * No dependency, no magic — just enough for the ~13 REST routes.
 */
export class Router {
  private routes: RouteDef[] = [];

  private add(method: string, path: string, authed: boolean, handler: RouteHandler): void {
    const { pattern, paramNames } = compilePath(path);
    this.routes.push({ method, pattern, paramNames, authed, handler });
  }

  get(path: string, authed: boolean, handler: RouteHandler): void {
    this.add("GET", path, authed, handler);
  }

  post(path: string, authed: boolean, handler: RouteHandler): void {
    this.add("POST", path, authed, handler);
  }

  match(
    method: string,
    pathname: string,
  ): { route: RouteDef; params: Record<string, string> } | null {
    for (const route of this.routes) {
      if (route.method !== method) continue;
      const m = route.pattern.exec(pathname);
      if (!m) continue;
      const params: Record<string, string> = {};
      for (let i = 0; i < route.paramNames.length; i++) {
        const raw = m[i + 1];
        params[route.paramNames[i]!] = raw === undefined ? "" : decodeURIComponent(raw);
      }
      return { route, params };
    }
    return null;
  }
}
