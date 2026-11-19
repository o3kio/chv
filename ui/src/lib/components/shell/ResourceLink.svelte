<script lang="ts">
	import { Box, Database, Network, Cpu } from 'lucide-svelte';

	interface Props {
		kind: string;
		id: string;
		name?: string;
		compact?: boolean;
	}

	let { kind, id, name, compact = false }: Props = $props();

	const label = $derived(name || id);
	
	const config = $derived.by(() => {
		const k = kind.toLowerCase();
		if (k.includes('vm')) return { icon: Box, href: `/vms/${id}`, label: 'VM' };
		if (k.includes('volume')) return { icon: Database, href: `/volumes/${id}`, label: 'Volume' };
		if (k.includes('network')) return { icon: Network, href: `/networks/${id}`, label: 'Network' };
		if (k.includes('node') || k.includes('host')) return { icon: Cpu, href: `/nodes/${id}`, label: 'Node' };
		return { icon: Box, href: '#', label: kind };
	});
</script>

<a href={config.href} class="inline-flex items-center gap-[0.35rem] no-underline text-(--shell-text) font-medium transition-colors duration-150 ease-in-out hover:text-(--shell-accent) {compact ? '' : ''}">
	<config.icon size={12} class="text-(--shell-text-muted)" />
	{#if !compact}
		<span class="text-[10px] uppercase font-bold text-(--shell-text-muted) tracking-wider">{config.label}</span>
	{/if}
	<span class="text-(length:--chv-text-sm) whitespace-nowrap overflow-hidden text-ellipsis max-w-[200px] {compact ? 'text-(length:--chv-text-xs)' : ''}">{label}</span>
</a>
