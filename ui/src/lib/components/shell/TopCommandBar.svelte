<script lang="ts">
	import type { PageDefinition } from '#lib/shell/app-shell.ts';
	import { Search } from 'lucide-svelte';
	import CommandPalette from './CommandPalette.svelte';

	interface Props {
		page: PageDefinition;
	}

	let { page }: Props = $props();
	let paletteOpen = $state(false);

	function openCommandPalette() {
		paletteOpen = true;
	}
</script>

<svelte:window
	onkeydown={(e) => {
		if ((e.metaKey || e.ctrlKey) && e.key === 'k') {
			e.preventDefault();
			openCommandPalette();
		}
	}}
/>

<div class="flex items-center justify-between gap-6 border border-(--border-subtle) rounded-sm bg-(--bg-surface) px-4 py-2 min-h-[48px] max-[960px]:flex-col max-[960px]:items-stretch max-[960px]:gap-3">
	<div class="flex flex-col gap-0.5">
		<div class="text-(length:--text-xs) font-bold tracking-[0.08em] uppercase text-(--shell-text-muted)">Control plane</div>
		<div class="text-(length:--text-sm) font-bold text-(--shell-text)">{page.navLabel}</div>
	</div>

	<button
		type="button"
		class="flex items-center gap-2.5 min-h-[36px] border border-(--border-subtle) rounded-sm bg-(--bg-surface-muted) px-3 w-[min(28rem,100%)] cursor-pointer transition-all duration-150 ease-in-out hover:border-primary hover:bg-[color-mix(in_srgb,var(--bg-surface-muted)_82%,var(--color-primary-light))] focus-visible:outline-hidden focus-visible:border-primary focus-visible:shadow-[0_0_0_3px_rgba(var(--color-primary-rgb),0.14)] max-[960px]:w-full"
		style="font-family: inherit"
		onclick={openCommandPalette}
	>
		<span class="inline-flex shrink-0 text-(--shell-text-muted)" aria-hidden="true">
			<Search size={14} />
		</span>
		<span class="flex-1 border-0 bg-transparent p-0 text-(length:--text-sm) font-medium text-(--shell-text-secondary) text-left">Search commands or jump to a resource</span>
		<div class="shrink-0 text-(length:--text-xs) font-bold text-(--shell-text-muted) bg-(--bg-surface) min-w-10 h-6 px-[0.45rem] grid place-items-center rounded-xs border border-(--border-subtle)">⌘K</div>
	</button>
</div>

<CommandPalette bind:open={paletteOpen} />
