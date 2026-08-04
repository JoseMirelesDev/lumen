<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { voice } from "$lib/stores/voice.svelte";
  import { auth } from "$lib/stores/auth.svelte";
  import { shell } from "$lib/stores/shell.svelte";

  // This component is keyed by channel id, so mount = join, unmount = leave.
  onMount(() => {
    const channel = shell.selectedChannel;
    if (channel && voice.channelId !== channel.id) void voice.join(channel);
  });
  onDestroy(() => {
    shell.dmCall = false;
    void voice.leave();
  });
</script>

<section class="voice">
  <header>
    <span class="name">🔊 {shell.selectedChannel?.name}</span>
    {#if voice.connected}
      <span class="status ok">connected — {voice.peers.length} peer(s)</span>
    {:else if voice.channelId}
      <span class="status">connecting…</span>
    {:else}
      <span class="status">not connected</span>
      <button
        class="join"
        onclick={() => {
          const c = shell.selectedChannel;
          if (c) void voice.join(c);
        }}
      >
        Join voice
      </button>
    {/if}
  </header>

  {#if voice.error}
    <p class="error">{voice.error}</p>
  {/if}

  <div class="grid">
    <div class="tile" class:speaking={voice.localSpeaking} class:muted={voice.muted && !voice.deafened}>
      <div class="avatar">
        {auth.user?.username.slice(0, 1).toUpperCase()}
        {#if voice.deafened}
          <span class="badge" title="Deafened">🔇</span>
        {:else if voice.muted}
          <span class="badge" title="Muted">🎙️</span>
        {/if}
      </div>
      <span class="label">{auth.user?.username} (you)</span>
      <div class="levelbar"><div style="width: {voice.localLevel * 100}%"></div></div>
    </div>

    {#each voice.peers as peer (peer.peerId)}
      <div class="tile" class:speaking={peer.speaking}>
        <div class="avatar">
          {peer.username.slice(0, 1).toUpperCase()}
          <span
            class="state"
            class:ok={peer.state === "connected"}
            class:bad={peer.state === "failed" || peer.state === "closed" || peer.state === "disconnected"}
            title={`voice: ${peer.state}`}
          >●</span>
        </div>
        <span class="label">{peer.username}</span>
        <div class="levelbar"><div style="width: {peer.level * 100}%"></div></div>
      </div>
    {/each}
  </div>

  <div class="callbar">
    <button
      class="ctrl"
      class:active={voice.muted}
      title={voice.muted ? "Unmute" : "Mute"}
      onclick={() => voice.toggleMute()}
    >
      {voice.muted ? "🎙️ muted" : "🎙️"}
    </button>
    <button
      class="ctrl"
      class:active={voice.deafened}
      title={voice.deafened ? "Undeafen" : "Deafen"}
      onclick={() => voice.toggleDeafen()}
    >
      {voice.deafened ? "🔇 deafened" : "🔇"}
    </button>
    <button class="ctrl leave" title="Leave voice" onclick={() => void voice.leave()}>
      Leave
    </button>
  </div>

  <details class="debug">
    <summary>Debug log ({voice.log.length})</summary>
    <pre>{#each voice.log as e}<div>{e.t} {e.msg}</div>{/each}</pre>
  </details>
</section>

<style>
  .voice {
    flex: 1;
    display: flex;
    flex-direction: column;
    background: var(--bg);
    min-width: 0;
  }
  header {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
  }
  .name {
    font-weight: 700;
    font-size: 14px;
  }
  .status {
    color: var(--text-dim);
    font-size: 12px;
  }
  .status.ok {
    color: #3ba55d;
  }
  .join {
    padding: 5px 12px;
    border-radius: 4px;
    border: none;
    background: var(--accent);
    color: #fff;
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
  }
  .error {
    margin: 0;
    padding: 8px 16px;
    color: var(--danger);
    font-size: 13px;
  }
  .grid {
    flex: 1;
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(140px, 1fr));
    gap: 14px;
    padding: 20px;
    overflow-y: auto;
    align-content: start;
  }
  .tile {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 6px;
    padding: 14px 8px;
    border-radius: 8px;
    background: var(--bg-raised);
    border: 2px solid transparent;
  }
  .tile.speaking {
    border-color: var(--accent);
  }
  .tile.muted .avatar {
    opacity: 0.5;
  }
  .avatar {
    position: relative;
    width: 56px;
    height: 56px;
    border-radius: 50%;
    display: grid;
    place-items: center;
    background: var(--bg-hover);
    color: var(--text);
    font-size: 22px;
    font-weight: 700;
  }
  .badge {
    position: absolute;
    bottom: -2px;
    right: -2px;
    font-size: 14px;
  }
  .state {
    position: absolute;
    bottom: 2px;
    right: 2px;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: #b9bbbe; /* connecting/new */
    color: transparent;
    font-size: 0;
  }
  .state.ok {
    background: #3ba55d; /* connected */
  }
  .state.bad {
    background: var(--danger); /* failed/closed/disconnected */
  }
  .label {
    font-size: 12px;
    font-weight: 600;
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .levelbar {
    width: 80%;
    height: 4px;
    border-radius: 2px;
    background: var(--bg);
    overflow: hidden;
  }
  .levelbar div {
    height: 100%;
    background: var(--accent);
    transition: width 60ms linear;
  }
  .callbar {
    display: flex;
    justify-content: center;
    gap: 10px;
    padding: 12px;
    border-top: 1px solid var(--border);
  }
  .ctrl {
    padding: 8px 16px;
    border-radius: 6px;
    border: none;
    background: var(--bg-hover);
    color: var(--text);
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
  }
  .ctrl.active {
    background: var(--accent);
    color: #fff;
  }
  .ctrl.leave {
    background: var(--danger);
    color: #fff;
  }
  .debug {
    border-top: 1px solid var(--border);
    background: var(--bg-raised);
    padding: 4px 12px;
  }
  .debug summary {
    cursor: pointer;
    font-size: 11px;
    color: var(--text-dim);
    padding: 4px 0;
  }
  .debug pre {
    font-size: 10px;
    max-height: 160px;
    overflow-y: auto;
    color: var(--text-dim);
    white-space: pre-wrap;
    word-break: break-word;
  }
</style>
