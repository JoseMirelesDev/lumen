/**
 * Backend URL resolution, in priority order:
 *  1. localStorage override (`lumen.backendUrl`) — set from the login screen,
 *     so a packaged build can point at any backend without a rebuild;
 *  2. `VITE_BACKEND_URL` at build time (e.g. a deployed worker URL);
 *  3. local dev default `http://localhost:8787`.
 */

const STORAGE_KEY = "lumen.backendUrl";

export function defaultBackendUrl(): string {
  return (import.meta.env.VITE_BACKEND_URL as string | undefined) ?? "http://localhost:8787";
}

export function getBackendUrl(): string {
  try {
    return localStorage.getItem(STORAGE_KEY) ?? defaultBackendUrl();
  } catch {
    return defaultBackendUrl();
  }
}

export function setBackendUrl(url: string): void {
  try {
    localStorage.setItem(STORAGE_KEY, url.replace(/\/+$/, ""));
  } catch {
    /* storage unavailable — in-memory only */
  }
}
