<script lang="ts">
  import { untrack } from "svelte";
  import { auth } from "$lib/stores/auth.svelte";
  import { shell } from "$lib/stores/shell.svelte";
  import { voice } from "$lib/stores/voice.svelte";
  import Login from "$lib/components/Login.svelte";
  import ServerRail from "$lib/components/ServerRail.svelte";
  import ChannelList from "$lib/components/ChannelList.svelte";
  import ChatView from "$lib/components/ChatView.svelte";
  import FriendsView from "$lib/components/FriendsView.svelte";
  import VoiceView from "$lib/components/VoiceView.svelte";

  // Boot: restore servers once we know who we are; reset on logout.
  // The mutations must run inside untrack(): shell.reset() and voice.leave()
  // synchronously read+write store $state (e.g. pushLog slices `log`), which
  // an effect would otherwise track as dependencies — every write would
  // re-run the effect, spinning into effect_update_depth_exceeded and a
  // frozen UI at boot (no session) and on logout. Track only auth.user.
  $effect(() => {
    const user = auth.user;
    untrack(() => {
      if (user) {
        void shell.loadServers();
        void shell.loadFriends();
      } else {
        shell.reset();
        void voice.leave();
      }
    });
  });

  // Voice lifecycle lives in VoiceView: it joins on mount (keyed by channel)
  // and leaves on unmount. No effect here — explicit and predictable.
</script>

{#if !auth.user}
  <Login />
{:else}
  <div class="shell">
    <ServerRail />
    {#if shell.view === "friends"}
      <FriendsView />
    {:else}
      <ChannelList />
    {/if}
    {#if shell.selectedChannel?.kind === "voice" || (shell.selectedChannel?.kind === "dm" && shell.dmCall)}
      {#key shell.selectedChannelId}
        <VoiceView />
      {/key}
    {:else}
      <ChatView />
    {/if}
  </div>
{/if}

<style>
  .shell {
    height: 100%;
    display: flex;
  }
</style>
