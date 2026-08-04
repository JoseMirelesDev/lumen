<script lang="ts">
  import { shell } from "$lib/stores/shell.svelte";
  import { auth } from "$lib/stores/auth.svelte";

  let draft = $state("");
  let sendError = $state<string | null>(null);

  function time(iso: string): string {
    const d = new Date(iso);
    return `${d.toLocaleDateString()} ${d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}`;
  }

  async function send() {
    const content = draft.trim();
    if (!content) return;
    sendError = null;
    try {
      await shell.sendMessage(content);
      draft = "";
    } catch (err) {
      sendError = err instanceof Error ? err.message : String(err);
    }
  }
</script>

<section class="chat">
  <header>
    {#if shell.selectedChannel?.kind === "dm"}
      <span class="hash">💬</span>
      <span class="name">Direct message</span>
      <button
        class="call"
        class:active={shell.dmCall}
        onclick={() => (shell.dmCall = !shell.dmCall)}
        title={shell.dmCall ? "Leave call" : "Start voice call"}
      >
        {shell.dmCall ? "Leave" : "🔊 Call"}
      </button>
    {:else}
      <span class="hash">#</span>
      <span class="name">{shell.selectedChannel?.name}</span>
    {/if}
  </header>

  <div class="messages">
    {#each shell.messages as message (message.id)}
      <div class="msg" class:self={message.authorId === auth.user?.id}>
        <span class="author">{message.authorName}</span>
        <span class="time">{time(message.createdAt)}</span>
        <p class="content">{message.content}</p>
      </div>
    {:else}
      <p class="empty">No messages yet.</p>
    {/each}
  </div>

  {#if shell.error}<p class="error">{shell.error}</p>{/if}

  <form class="composer" onsubmit={(e) => { e.preventDefault(); send(); }}>
    <input
      bind:value={draft}
      placeholder={`Message #${shell.selectedChannel?.name ?? ""}`}
      maxlength="2000"
      autocomplete="off"
    />
    <button type="submit" disabled={!draft.trim() || shell.loading}>Send</button>
  </form>
  {#if sendError}<p class="error send">{sendError}</p>{/if}
</section>

<style>
  .chat {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-width: 0;
    background: var(--bg);
  }
  header {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
  }
  .hash {
    color: var(--text-dim);
    font-weight: 700;
  }
  .name {
    font-weight: 700;
    font-size: 14px;
  }
  .call {
    margin-left: auto;
    padding: 5px 12px;
    border: none;
    border-radius: 4px;
    background: var(--bg-hover);
    color: var(--text);
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
  }
  .call:hover,
  .call.active {
    background: var(--accent);
    color: #fff;
  }
  .messages {
    flex: 1;
    overflow-y: auto;
    padding: 12px 16px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .msg {
    display: grid;
    grid-template-columns: auto auto 1fr;
    gap: 4px 10px;
  }
  .msg.self .author {
    color: var(--accent);
  }
  .author {
    font-weight: 700;
    font-size: 13px;
  }
  .time {
    color: var(--text-dim);
    font-size: 11px;
    padding-top: 2px;
  }
  .content {
    grid-column: 1 / -1;
    margin: 0;
    font-size: 14px;
    line-height: 1.45;
    white-space: pre-wrap;
    word-break: break-word;
  }
  .empty {
    color: var(--text-dim);
    font-size: 13px;
  }
  .composer {
    display: flex;
    gap: 8px;
    padding: 10px 16px;
    border-top: 1px solid var(--border);
  }
  .composer input {
    flex: 1;
    padding: 9px 12px;
    border-radius: 4px;
    border: 1px solid var(--border);
    background: var(--bg-raised);
    color: var(--text);
    font-size: 14px;
  }
  .composer input:focus {
    outline: none;
    border-color: var(--accent);
  }
  .composer button {
    padding: 0 16px;
    border-radius: 4px;
    border: none;
    background: var(--accent);
    color: #fff;
    font-weight: 600;
    cursor: pointer;
  }
  .composer button:disabled {
    opacity: 0.6;
  }
  .error {
    margin: 0;
    padding: 0 16px;
    color: var(--danger);
    font-size: 13px;
  }
  .error.send {
    padding-bottom: 4px;
  }
</style>
