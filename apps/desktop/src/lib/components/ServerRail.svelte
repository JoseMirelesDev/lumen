<script lang="ts">
  import { shell } from "$lib/stores/shell.svelte";
  import { auth } from "$lib/stores/auth.svelte";

  let showCreate = $state(false);
  let showJoin = $state(false);
  let name = $state("");
  let inviteCode = $state("");
  let actionError = $state<string | null>(null);

  async function create() {
    actionError = null;
    try {
      await shell.createServer(name.trim());
      name = "";
      showCreate = false;
    } catch (err) {
      actionError = err instanceof Error ? err.message : String(err);
    }
  }

  async function join() {
    actionError = null;
    try {
      await shell.joinServer(inviteCode);
      inviteCode = "";
      showJoin = false;
    } catch (err) {
      actionError = err instanceof Error ? err.message : String(err);
    }
  }
</script>

<nav class="rail">
  {#each shell.servers as entry (entry.server.id)}
    <button
      class="server"
      class:active={shell.selectedServerId === entry.server.id}
      title={entry.server.name}
      onclick={() => shell.selectServer(entry.server.id)}
    >
      {entry.server.name.slice(0, 1).toUpperCase()}
    </button>
  {/each}

  <button class="server add" title="Create server" onclick={() => (showCreate = true)}>+</button>
  <button class="server add" title="Join by invite" onclick={() => (showJoin = true)}>@</button>

  <div class="spacer"></div>

  <button class="server user" title={`${auth.user?.username} — sign out`} onclick={() => auth.logout()}>
    {auth.user?.username.slice(0, 1).toUpperCase()}
  </button>
</nav>

{#if showCreate}
  <div class="overlay" role="dialog" aria-label="Create server">
    <form class="modal" onsubmit={(e) => { e.preventDefault(); create(); }}>
      <h2>Create server</h2>
      <input bind:value={name} placeholder="Server name" maxlength="100" />
      {#if actionError}<p class="error">{actionError}</p>{/if}
      <div class="actions">
        <button type="button" class="ghost" onclick={() => (showCreate = false)}>Cancel</button>
        <button type="submit" disabled={!name.trim()}>Create</button>
      </div>
    </form>
  </div>
{/if}

{#if showJoin}
  <div class="overlay" role="dialog" aria-label="Join server">
    <form class="modal" onsubmit={(e) => { e.preventDefault(); join(); }}>
      <h2>Join server</h2>
      <input bind:value={inviteCode} placeholder="Invite code" spellcheck="false" />
      {#if actionError}<p class="error">{actionError}</p>{/if}
      <div class="actions">
        <button type="button" class="ghost" onclick={() => (showJoin = false)}>Cancel</button>
        <button type="submit" disabled={!inviteCode.trim()}>Join</button>
      </div>
    </form>
  </div>
{/if}

<style>
  .rail {
    width: 56px;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 6px;
    padding: 8px 0;
    background: var(--bg-rail);
    overflow-y: auto;
  }
  .server {
    width: 40px;
    height: 40px;
    border-radius: 50%;
    border: none;
    background: var(--bg-raised);
    color: var(--text);
    font-weight: 700;
    font-size: 18px;
    cursor: pointer;
    transition: border-radius 0.1s;
  }
  .server:hover {
    border-radius: 12px;
  }
  .server.active {
    border-radius: 12px;
    background: var(--accent);
    color: #fff;
  }
  .server.add {
    color: var(--accent);
    background: transparent;
    border: 1px dashed var(--border);
    font-size: 20px;
  }
  .server.user {
    font-size: 14px;
    background: var(--bg-raised);
    color: var(--text-dim);
  }
  .spacer {
    flex: 1;
  }
  .overlay {
    position: fixed;
    inset: 0;
    display: grid;
    place-items: center;
    background: rgba(0, 0, 0, 0.6);
    z-index: 10;
  }
  .modal {
    width: 300px;
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 20px;
    border-radius: 8px;
    background: var(--bg-raised);
  }
  .modal h2 {
    margin: 0;
    font-size: 16px;
  }
  .modal input {
    padding: 8px 10px;
    border-radius: 4px;
    border: 1px solid var(--border);
    background: var(--bg);
    color: var(--text);
  }
  .modal input:focus {
    outline: none;
    border-color: var(--accent);
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
  .actions button {
    padding: 6px 12px;
    border-radius: 4px;
    border: none;
    background: var(--accent);
    color: #fff;
    font-weight: 600;
    cursor: pointer;
  }
  .actions .ghost {
    background: none;
    color: var(--text-dim);
  }
  .error {
    margin: 0;
    color: var(--danger);
    font-size: 13px;
  }
</style>
