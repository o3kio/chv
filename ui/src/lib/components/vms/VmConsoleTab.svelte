<script lang="ts">
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import { Terminal } from 'lucide-svelte';

	interface ConsoleConnection {
		url: string;
		token: string;
	}

	interface Props {
		vmId: string;
		consoleLoading: boolean;
		liveConsoleUrl: string | undefined;
		liveConsoleToken: string | undefined;
		VmConsoleComponent: typeof import('#lib/components/vms/VmConsole.svelte').default | null;
		running: boolean;
		getConsoleUrl: () => Promise<ConsoleConnection>;
		consoleExpiresAt?: string;
	}

	let {
		vmId,
		consoleLoading,
		liveConsoleUrl,
		liveConsoleToken,
		VmConsoleComponent,
		running,
		getConsoleUrl,
		consoleExpiresAt
	}: Props = $props();
</script>

<SectionCard title="Direct Fabric Console" icon={Terminal}>
	{#if consoleLoading}
		<p class="empty-hint">Establishing encrypted bypass tunnel...</p>
	{:else if liveConsoleUrl && VmConsoleComponent}
		<VmConsoleComponent
			{vmId}
			consoleUrl={liveConsoleUrl}
			consoleToken={liveConsoleToken}
			{running}
			{getConsoleUrl}
			consoleExpiresAt={consoleExpiresAt}
		/>
	{:else if liveConsoleUrl}
		<p class="empty-hint">Loading console workspace...</p>
	{:else}
		<p class="empty-hint">Console registry inaccessible. Instance state may prevent access.</p>
	{/if}
</SectionCard>

<style>
	.empty-hint {
		font-size: var(--text-xs);
		color: var(--shell-text-muted);
		text-align: center;
		padding: 1rem 0;
	}
</style>
