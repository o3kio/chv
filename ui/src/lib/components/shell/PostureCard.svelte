<script lang="ts">
	import { ShieldCheck, AlertTriangle } from 'lucide-svelte';

	interface Props {
		status: string;
		actual_state?: string;
	}

	let { status, actual_state }: Props = $props();

	const isHealthy = $derived(status === 'online' || actual_state === 'running');
</script>

<div class="flex items-center gap-3 p-3 rounded-xs {isHealthy ? 'border-l-2 border-success' : 'border-l-2 border-warning'}" style={isHealthy ? 'background: rgba(var(--color-success-rgb), 0.08)' : 'background: rgba(var(--color-warning-rgb), 0.08)'}>
	{#if isHealthy}
		<ShieldCheck size={14} class="text-success" />
		<div class="flex flex-col gap-0.5">
			<span class="text-[10px] font-extrabold text-(--chv-color-neutral-900)">STATUS: {(status || actual_state || 'unknown').toUpperCase()}</span>
			<span class="text-[10px] text-(--chv-color-neutral-500)">Reported by the inspected resource record.</span>
		</div>
	{:else}
		<AlertTriangle size={14} class="text-warning" />
		<div class="flex flex-col gap-0.5">
			<span class="text-[10px] font-extrabold text-(--chv-color-neutral-900)">STATUS: {(status || actual_state || 'unknown').toUpperCase()}</span>
			<span class="text-[10px] text-(--chv-color-neutral-500)">Inspected record is not reporting a healthy/running state.</span>
		</div>
	{/if}
</div>
