<script lang="ts">
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import { FileText } from 'lucide-svelte';

	interface Props {
		bootLogLoading: boolean;
		bootLog: string;
	}

	let { bootLogLoading, bootLog }: Props = $props();
</script>

<SectionCard title="Serial Boot Sequence" icon={FileText}>
	{#if bootLogLoading}
		<p class="empty-hint">Streaming boot sequence records...</p>
	{:else}
		<pre class="boot-log">{bootLog}</pre>
	{/if}
</SectionCard>

<style>
	.empty-hint {
		font-size: var(--chv-text-xs);
		color: var(--shell-text-muted);
		text-align: center;
		padding: 1rem 0;
	}

	.boot-log {
		font-family: var(--chv-font-mono);
		font-size: var(--chv-text-xs);
		line-height: 1.5;
		background: var(--chv-color-neutral-50);
		border: 1px solid var(--shell-line);
		border-radius: var(--radius-md);
		padding: var(--space-4);
		overflow-x: auto;
		max-height: 600px;
		overflow-y: auto;
		white-space: pre;
		color: var(--shell-text);
		margin: 0;
	}
</style>
