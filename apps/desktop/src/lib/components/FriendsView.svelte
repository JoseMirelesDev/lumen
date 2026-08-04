<script lang="ts">
  import { shell, isOnline } from "$lib/stores/shell.svelte";

  let requestUser = $state("");
  let actionError = $state<string | null>(null);
  let sending = $state(false);

  async function sendRequest() {
    const username = requestUser.trim();
    if (!username || sending) return;
    sending = true;
    actionError = null;
    try {
      await shell.sendFriendRequest(username);
      requestUser = "";
    } catch (err) {
      actionError = err instanceof Error ? err.message : String(err);
    } finally {
      sending = false;
    }
  }

  function dmTitle(other: string): string {
    return `Direct message with ${other}`;
  }
</script>

<aside class="list">
  <header>
    <span class="name">Friends</span>
  </header>

  <div class="group">
    <span class="group-label">Direct messages</span>
    {#if shell.dmList.length === 0}
      <p class="empty">No DMs yet — send a friend request below.</p>
    {/if}
    {#each shell.dmList as dm (dm.channel.id)}
      <button
        class="channel"
        class:active={shell.selectedChannelId === dm.channel.id}
        onclick={() => shell.selectChannel(dm.channel.id)}
      >
        <span class="icon">💬</span>
        <span class="name">{dm.otherUsername}</span>
      </button>
    {/each}
  </div>

  <div class="group">
    <span class="group-label">Online</span>
    {#each shell.friends.filter((f) => isOnline(f.user)) as friend (friend.user.id)}
      <button
        class="channel friend"
        onclick={() => shell.openDm(friend.user.username)}
      >
        <span class="dot online"></span>
        <span class="name">{friend.user.username}</span>
      </button>
    {:else}
      <p class="empty">No friends online.</p>
    {/each}
  </div>

  <div class="group">
    <span class="group-label">Offline</span>
    {#each shell.friends.filter((f) => !isOnline(f.user)) as friend (friend.user.id)}
      <button
        class="channel friend"
        onclick={() => shell.openDm(friend.user.username)}
      >
        <span class="dot offline"></span>
        <span class="name">{friend.user.username}</span>
      </button>
    {:else}
      <p class="empty">No friends yet.</p>
    {/each}
  </div>

  {#if shell.pending.length > 0}
    <div class="group">
      <span class="group-label">Requests</span>
      {#each shell.pending.filter((p) => p.direction === "incoming") as req (req.id)}
        <div class="request">
          <span class="name">{req.user.username}</span>
          <div class="actions">
            <button class="mini accept" onclick={() => shell.acceptFriend(req.id)}>✓</button>
            <button class="mini decline" onclick={() => shell.declineFriend(req.id)}>✕</button>
          </div>
        </div>
      {/each}
    </div>
  {/if}

  <form class="request-box" onsubmit={(e) => { e.preventDefault(); sendRequest(); }}>
    <input bind:value={requestUser} placeholder="Add friend by username" maxlength="32" />
    <button type="submit" disabled={!requestUser.trim() || sending}>Send</button>
  </form>
  {#if actionError}<p class="error">{actionError}</p>{/if}
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
  .channel.friend {
    font-weight: 500;
  }
  .icon {
    width: 14px;
    text-align: center;
    font-size: 12px;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex: none;
  }
  .dot.online {
    background: #3ba55d;
  }
  .dot.offline {
    background: var(--text-dim);
    opacity: 0.6;
  }
  .empty {
    margin: 0;
    padding: 4px 8px;
    font-size: 12px;
    color: var(--text-dim);
  }
  .request {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    padding: 6px 8px;
    font-size: 13px;
    color: var(--text);
  }
  .request .name {
    font-weight: 500;
  }
  .request .actions {
    display: flex;
    gap: 4px;
  }
  .mini {
    width: 22px;
    height: 22px;
    border: none;
    border-radius: 4px;
    background: var(--bg-hover);
    color: var(--text);
    cursor: pointer;
    font-size: 12px;
  }
  .mini.accept:hover {
    background: #3ba55d;
  }
  .mini.decline:hover {
    background: var(--danger);
  }
  .request-box {
    display: flex;
    gap: 6px;
    margin-top: auto;
    padding: 10px;
    border-top: 1px solid var(--border);
  }
  .request-box input {
    flex: 1;
    min-width: 0;
    padding: 6px 8px;
    border-radius: 4px;
    border: 1px solid var(--border);
    background: var(--bg);
    color: var(--text);
    font-size: 12px;
  }
  .request-box input:focus {
    outline: none;
    border-color: var(--accent);
  }
  .request-box button {
    padding: 6px 10px;
    border: none;
    border-radius: 4px;
    background: var(--accent);
    color: #fff;
    font-weight: 600;
    font-size: 12px;
    cursor: pointer;
  }
  .request-box button:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .error {
    margin: 0;
    padding: 0 10px 8px;
    color: var(--danger);
    font-size: 12px;
  }
</style>
