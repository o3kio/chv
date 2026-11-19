<script lang="ts">
	import StatusBadge from './StatusBadge.svelte';
	import type { ShellTone } from '#lib/shell/app-shell.ts';
	import { ChevronRight } from 'lucide-svelte';

	interface Props {
		title: string;
		eyebrow?: string;
		tone?: ShellTone;
		statusLabel?: string;
		description?: string;
		parentHref?: string;
		parentLabel?: string;
		actions?: import('svelte').Snippet;
	}

	let { title, eyebrow, tone = 'unknown', statusLabel, description, parentHref, parentLabel, actions }: Props = $props();
</script>

<header class="flex justify-between items-start py-4 px-0 border-b border-(--shell-line) mb-4 gap-8 max-[768px]:flex-col max-[768px]:gap-4">
	<div class="flex flex-col gap-1 flex-1">
		{#if parentHref && parentLabel}
			<nav class="flex items-center gap-1 text-(length:--chv-text-xs) text-(--shell-text-muted)">
				<a href={parentHref} class="text-inherit no-underline hover:text-(--shell-text)">{parentLabel}</a>
				<ChevronRight size={12} class="opacity-50" />
			</nav>
		{/if}
		
		<div class="flex items-baseline gap-3">
			<div class="flex flex-col">
				{#if eyebrow}<span class="text-(length:--chv-text-xs) font-bold uppercase tracking-wider text-(--shell-text-muted)">{eyebrow}</span>{/if}
				<h1 class="text-(length:--chv-text-2xl) font-bold text-(--shell-text) m-0 leading-[1.1]">{title}</h1>
			</div>
			
			<div>
				<StatusBadge label={statusLabel || 'unknown'} {tone} />
			</div>
		</div>

		{#if description}
			<p class="text-(length:--chv-text-sm) text-(--shell-text-muted) mt-1 mb-0 max-w-[600px]">{description}</p>
		{/if}
	</div>

	{#if actions}
		<div class="flex gap-2 pt-1">
			{@render actions()}
		</div>
	{/if}
</header>
