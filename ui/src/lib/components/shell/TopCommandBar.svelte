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

<div class="flex items-center justify-between gap-6 border border-[var(--border-subtle)] rounded-[var(--radius-sm)] bg-[var(--bg-surface)] px-4 py-2 min-h-[48px] max-[960px]:flex-col max-[960px]:items-stretch max-[960px]:gap-3">
	<div class="flex flex-col gap-[0.125rem]">
		<div class="text-[length:var(--text-xs)] font-bold tracking-[0.08em] uppercase text-[var(--shell-text-muted)]">Control plane</div>
		<div class="text-[length:var(--text-sm)] font-bold text-[var(--shell-text)]">{page.navLabel}</div>
	</div>

	<button
		type="button"
		class="flex items-center gap-[0.625rem] min-h-[36px] border border-[var(--border-subtle)] rounded-[var(--radius-sm)] bg-[var(--bg-surface-muted)] px-3 w-[min(28rem,100%)] cursor-pointer transition-all duration-150 ease-in-out hover:border-[var(--color-primary)] hover:bg-[color-mix(in_srgb,var(--bg-surface-muted)_82%,var(--color-primary-light))] focus-visible:outline-none focus-visible:border-[var(--color-primary)] focus-visible:shadow-[0_0_0_3px_rgba(var(--color-primary-rgb),0.14)] max-[960px]:w-full"
		style="font-family: inherit"
		onclick={openCommandPalette}
	>
		<span class="inline-flex shrink-0 text-[var(--shell-text-muted)]" aria-hidden="true">
			<Search size={14} />
		</span>
		<span class="flex-1 border-0 bg-transparent p-0 text-[length:var(--text-sm)] font-medium text-[var(--shell-text-secondary)] text-left">Search commands or jump to a resource</span>
		<div class="shrink-0 text-[length:var(--text-xs)] font-bold text-[var(--shell-text-muted)] bg-[var(--bg-surface)] min-w-[2.5rem] h-6 px-[0.45rem] grid place-items-center rounded-[var(--radius-xs)] border border-[var(--border-subtle)]">⌘K</div>
	</button>
</div>

<CommandPalette bind:open={paletteOpen} />
