import type { Channel, ServerWithChannels, TextMessage } from "@lumen/protocol";
import { auth } from "./auth.svelte";

/**
 * Navigation + data store for the main shell: the server list, the selected
 * server/channel, and the message history of the selected text channel.
 */
class ShellStore {
  servers = $state<ServerWithChannels[]>([]);
  selectedServerId = $state<string | null>(null);
  selectedChannelId = $state<string | null>(null);
  messages = $state<TextMessage[]>([]);
  loading = $state(false);
  error = $state<string | null>(null);

  selectedServer = $derived(
    this.servers.find((s) => s.server.id === this.selectedServerId) ?? null,
  );
  selectedChannel = $derived(
    this.selectedServer?.channels.find((c) => c.id === this.selectedChannelId) ?? null,
  );

  async loadServers(): Promise<void> {
    this.loading = true;
    this.error = null;
    try {
      this.servers = await auth.api.listServers();
      // Restore selection if the server/channel still exists, else pick defaults.
      if (!this.selectedServer) {
        this.selectedServerId = this.servers[0]?.server.id ?? null;
      }
      if (!this.selectedChannel) {
        const firstText = this.selectedServer?.channels.find((c) => c.kind === "text");
        this.selectedChannelId = firstText?.id ?? this.selectedServer?.channels[0]?.id ?? null;
      }
      if (this.selectedChannel?.kind === "text") await this.loadMessages();
    } catch (err) {
      this.error = err instanceof Error ? err.message : String(err);
    } finally {
      this.loading = false;
    }
  }

  async selectServer(serverId: string): Promise<void> {
    this.selectedServerId = serverId;
    const server = this.servers.find((s) => s.server.id === serverId);
    const firstText = server?.channels.find((c) => c.kind === "text");
    this.selectedChannelId = firstText?.id ?? server?.channels[0]?.id ?? null;
    if (this.selectedChannel?.kind === "text") await this.loadMessages();
  }

  async selectChannel(channelId: string): Promise<void> {
    this.selectedChannelId = channelId;
    if (this.selectedChannel?.kind === "text") await this.loadMessages();
  }

  async loadMessages(): Promise<void> {
    if (!this.selectedChannelId) return;
    this.error = null;
    try {
      this.messages = await auth.api.listMessages(this.selectedChannelId);
    } catch (err) {
      this.error = err instanceof Error ? err.message : String(err);
    }
  }

  async sendMessage(content: string): Promise<void> {
    if (!this.selectedChannelId || !content.trim()) return;
    await auth.api.postMessage(this.selectedChannelId, content);
    await this.loadMessages();
  }

  async createServer(name: string): Promise<void> {
    const { server } = await auth.api.createServer(name);
    await this.loadServers();
    this.selectedServerId = server.id;
    await this.selectServer(server.id);
  }

  async joinServer(inviteCode: string): Promise<void> {
    const { server } = await auth.api.joinServer(inviteCode.trim());
    await this.loadServers();
    this.selectedServerId = server.id;
    await this.selectServer(server.id);
  }


  async createChannel(name: string, kind: "text" | "voice"): Promise<void> {
    if (!this.selectedServerId) return;
    const { channel } = await auth.api.createChannel(this.selectedServerId, name, kind);
    await this.loadServers();
    this.selectedChannelId = channel.id;
    if (channel.kind === "text") await this.loadMessages();
  }

  reset(): void {
    this.servers = [];
    this.selectedServerId = null;
    this.selectedChannelId = null;
    this.messages = [];
  }
}

export const shell = new ShellStore();
