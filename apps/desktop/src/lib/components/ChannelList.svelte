<script lang="ts">
  import type { Channel } from "@lumen/protocol";
  import { shell } from "$lib/stores/shell.svelte";

  let showCreate = $state(false);
  let channelName = $state("");
  let channelKind = $state<"text" | "voice">("text");
  let actionError = $state<string | null>(null);
  let copied = $state(false);

  /** Copy the selected server's invite code (created with the server). */
  async function copyInvite() {
    const code = shell.selectedServer?.server.inviteCode;
    if (!code) return;
    try {
      await navigator.clipboard.writeText(code);
    } catch {
      // WebKitGTK's async clipboard can reject without a granted permission;
      // fall back to a synchronous select+execCommand copy.
      const ta = document.createElement("textarea");
      ta.value = code;
      document.body.appendChild(ta);
      ta.select();
      document.execCommand("copy");
      ta.remove();
    }
    copied = true;
    setTimeout(() => (copied = false), 1500);
  }

  async function create() {
    actionError = null;
    try {
      await shell.createChannel(channelName.trim(), channelKind);
      channelName = "";
      showCreate = false;
    } catch (err) {
      actionError = err instanceof Error ? err.message : String(err);
    }
  }

  function icon(kind: Channel["kind"]): string {
    return kind === "voice" ? "🔊" : "#";
  }
</script>

<aside class="list">
  <header>
    <span class="name" title={shell.selectedServer?.server.name}>{shell.selectedServer?.server.name}</span>
    <button
      class="invite"
      title={
        shell.selectedServer
          ? `Invite code: ${shell.selectedServer.server.inviteCode} — click to copy`
          : "Invite"
      }
      disabled={!shell.selectedServer}
      onclick={copyInvite}
    >{copied ? "✓" : "🔗"}</button>
    <button class="add" title="Create channel" onclick={() => (showCreate = true)}>+</button>
  </header>

  <div class="group">
    <span class="group-label">Text</span>
    {#each shell.selectedServer?.channels.filter((c) => c.kind === "text") ?? [] as channel (channel.id)}
      <button
        class="channel"
        class:active={shell.selectedChannelId === channel.id}
        onclick={() => shell.selectChannel(channel.id)}
      >
        <span class="icon">{icon(channel.kind)}</span>
        {channel.name}
      </button>
    {/each}
  </div>

  <div class="group">
    <span class="group-label">Voice</span>
    {#each shell.selectedServer?.channels.filter((c) => c.kind === "voice") ?? [] as channel (channel.id)}
      <button
        class="channel"
        class:active={shell.selectedChannelId === channel.id}
        onclick={() => shell.selectChannel(channel.id)}
      >
        <span class="icon">{icon(channel.kind)}</span>
        {channel.name}
      </button>
    {/each}
  </div>

  {#if showCreate}
    <div class="overlay" role="dialog" aria-label="Create channel">
      <form class="modal" onsubmit={(e) => { e.preventDefault(); create(); }}>
        <h2>Create channel</h2>
        <input bind:value={channelName} placeholder="Channel name" maxlength="50" />
        <div class="kinds">
          <label><input type="radio" bind:group={channelKind} value="text" /> Text</label>
          <label><input type="radio" bind:group={channelKind} value="voice" /> Voice</label>
        </div>
        {#if actionError}<p class="error">{actionError}</p>{/if}
        <div class="actions">
          <button type="button" class="ghost" onclick={() => (showCreate = false)}>Cancel</button>
          <button type="submit" disabled={!channelName.trim()}>Create</button>
        </div>
      </form>
    </div>
  {/if}
</aside>

<style>
  .list {
    width: 220px;
    display: flex;
    flex-direction: column;
    background: var(--bg-list);
    overflow-y: auto;
  }
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 12px 14px;
    border-bottom: 1px solid var(--border);
  }
  .name {
    font-weight: 700;
    font-size: 14px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .add {
    border: none;
    background: none;
    color: var(--text-dim);
    font-size: 16px;
    cursor: pointer;
  }
  .add:hover {
    color: var(--text);
  }
  .invite {
    border: none;
    background: none;
    color: var(--text-dim);
    font-size: 14px;
    cursor: pointer;
    margin-left: auto;
    margin-right: 6px;
  }
  .invite:hover:not(:disabled) {
    color: var(--text);
  }
  .invite:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .group {
    padding: 8px 6px;
  }
  .group-label {
    display: block;
    padding: 2px 8px;
    font-size: 11px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--text-dim);
  }
  .channel {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 6px 8px;
    border: none;
    border-radius: 4px;
    background: none;
    color: var(--text-dim);
    font-size: 13px;
    text-align: left;
    cursor: pointer;
  }
  .channel:hover {
    background: var(--bg-hover);
    color: var(--text);
  }
  .channel.active {
    background: var(--bg-hover);
    color: var(--text);
  }
  .icon {
    width: 14px;
    text-align: center;
    font-size: 12px;
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
    width: 280px;
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
  .kinds {
    display: flex;
    gap: 14px;
    font-size: 13px;
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
