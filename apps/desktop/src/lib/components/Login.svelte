<script lang="ts">
  import { auth } from "$lib/stores/auth.svelte";

  let mode = $state<"login" | "register">("login");
  let username = $state("");
  let password = $state("");
  let backendUrl = $state(auth.backendUrl);

  async function submit() {
    const ok =
      mode === "login"
        ? await auth.login(username.trim(), password)
        : await auth.register(username.trim(), password);
    if (ok) {
      username = "";
      password = "";
    }
  }

  async function applyBackendUrl() {
    await auth.setBackendUrl(backendUrl);
  }
</script>

<div class="login">
  <form onsubmit={(e) => { e.preventDefault(); submit(); }}>
    <h1>Lumen</h1>
    <p class="tagline">ultralight voice for small groups</p>

    <label>
      Username
      <input bind:value={username} autocomplete="username" required minlength="3" maxlength="32" />
    </label>
    <label>
      Password
      <input
        bind:value={password}
        type="password"
        autocomplete={mode === "login" ? "current-password" : "new-password"}
        required
        minlength="8"
      />
    </label>

    {#if auth.error}
      <p class="error">{auth.error}</p>
    {/if}

    <button type="submit" disabled={auth.busy}>
      {mode === "login" ? "Sign in" : "Create account"}
    </button>
    <button type="button" class="link" onclick={() => (mode = mode === "login" ? "register" : "login")}>
      {mode === "login" ? "No account? Register" : "Have an account? Sign in"}
    </button>

    <details class="backend">
      <summary>Backend URL</summary>
      <div class="row">
        <input bind:value={backendUrl} spellcheck="false" />
        <button type="button" onclick={applyBackendUrl}>Apply</button>
      </div>
    </details>
  </form>
</div>

<style>
  .login {
    height: 100%;
    display: grid;
    place-items: center;
    background: var(--bg);
  }
  form {
    width: 320px;
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 32px;
    border-radius: 8px;
    background: var(--bg-raised);
  }
  h1 {
    margin: 0;
    font-size: 28px;
    letter-spacing: 0.5px;
  }
  .tagline {
    margin: -6px 0 8px;
    color: var(--text-dim);
    font-size: 13px;
  }
  label {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 12px;
    font-weight: 600;
    color: var(--text-dim);
  }
  input {
    padding: 8px 10px;
    border-radius: 4px;
    border: 1px solid var(--border);
    background: var(--bg);
    color: var(--text);
    font-size: 14px;
  }
  input:focus {
    outline: none;
    border-color: var(--accent);
  }
  button {
    padding: 8px 12px;
    border-radius: 4px;
    border: none;
    background: var(--accent);
    color: #fff;
    font-weight: 600;
    cursor: pointer;
  }
  button:disabled {
    opacity: 0.6;
  }
  button.link {
    background: none;
    color: var(--text-dim);
    font-weight: 500;
  }
  button.link:hover {
    color: var(--text);
  }
  .error {
    margin: 0;
    color: var(--danger);
    font-size: 13px;
  }
  .backend {
    margin-top: 8px;
    font-size: 12px;
    color: var(--text-dim);
  }
  .backend summary {
    cursor: pointer;
  }
  .row {
    display: flex;
    gap: 6px;
    margin-top: 6px;
  }
  .row input {
    flex: 1;
    font-size: 12px;
  }
</style>
