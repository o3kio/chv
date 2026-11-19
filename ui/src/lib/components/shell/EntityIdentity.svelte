<script lang="ts">
	import { Server, Box, Database } from 'lucide-svelte';

	interface Props {
		type: string;
		label: string;
		id: string;
	}

	let { type, label, id }: Props = $props();

	const iconClass = $derived(
		type === 'node'
			? 'text-primary bg-[rgba(var(--color-primary-rgb),0.1)]'
			: type === 'vm'
				? 'text-(--color-accent) bg-[rgba(var(--color-accent-rgb),0.1)]'
				: 'text-(--chv-color-neutral-500)'
	);
</script>

<section class="flex items-center gap-3">
	<div class="w-10 h-10 grid place-items-center bg-(--bg-surface-muted) border border-(--border-subtle) rounded-xs {iconClass}">
		{#if type === 'node'}<Server size={18} />
		{:else if type === 'vm'}<Box size={18} />
		{:else}<Database size={18} />{/if}
	</div>
	<div class="flex flex-col gap-0.5">
		<h3 class="text-sm font-extrabold text-(--chv-color-neutral-900) m-0 leading-none">{label}</h3>
		<span class="text-[9px] font-bold text-(--chv-color-neutral-400) font-mono">ID // {id.slice(0, 12)}</span>
	</div>
</section>
