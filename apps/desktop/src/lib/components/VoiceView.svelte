<script lang="ts">
  import { shell } from "$lib/stores/shell.svelte";
</script>

<section class="voice">
  <div class="pane">
    <h2>🔊 {shell.selectedChannel?.name}</h2>
    <p class="hint">
      Voice channels come alive in Fase 3: WebRTC mesh + RNNoise noise suppression.
      This pane will hold the connected members and the call bar.
    </p>
  </div>
</section>

<style>
  .voice {
    flex: 1;
    display: grid;
    place-items: center;
    background: var(--bg);
  }
  .pane {
    max-width: 420px;
    text-align: center;
    color: var(--text-dim);
  }
  .pane h2 {
    color: var(--text);
  }
  .hint {
    font-size: 13px;
    line-height: 1.5;
  }
</style>
