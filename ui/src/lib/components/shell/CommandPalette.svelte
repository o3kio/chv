<script lang="ts">
	import { goto } from '$app/navigation';
	import { tick } from 'svelte';
	import { Box, Server, Network, HardDrive, Settings, Activity, Plus, Search } from 'lucide-svelte';

	interface Command { id: string; title: string; category: string; href: string; icon: typeof Box; }
	interface Props { open?: boolean; }
	let { open = $bindable(false) }: Props = $props();

	const commands: Command[] = [
		{ id: 'go-vms', title: 'Go to VMs', category: 'VMs', href: '/vms', icon: Box },
		{ id: 'go-nodes', title: 'Go to Nodes', category: 'Nodes', href: '/nodes', icon: Server },
		{ id: 'go-networks', title: 'Go to Networks', category: 'Nodes', href: '/networks', icon: Network },
		{ id: 'go-volumes', title: 'Go to Volumes', category: 'Nodes', href: '/volumes', icon: HardDrive },
		{ id: 'go-settings', title: 'Go to Settings', category: 'Settings', href: '/settings', icon: Settings },
		{ id: 'go-alerts', title: 'Go to Alerts', category: 'Settings', href: '/alerts', icon: Activity },
		{ id: 'create-vm', title: 'Create VM', category: 'Actions', href: '/vms', icon: Plus },
		{ id: 'create-network', title: 'Create Network', category: 'Actions', href: '/networks', icon: Plus }
	];

	let query = $state('');
	let selectedIndex = $state(0);
	let inputRef = $state<HTMLInputElement | null>(null);
	let resultsRef = $state<HTMLDivElement | null>(null);
	let isVisible = $state(false);
	let isClosing = $state(false);

	let filtered = $derived(
		query.trim() === '' ? commands : commands.filter((c) =>
			c.title.toLowerCase().includes(query.toLowerCase()) || c.category.toLowerCase().includes(query.toLowerCase())
		)
	);

	function buildGroups(list: Command[]) {
		const map = new Map<string, Command[]>();
		for (const cmd of list) { const existing = map.get(cmd.category) ?? []; existing.push(cmd); map.set(cmd.category, existing); }
		return Array.from(map.entries());
	}
	let grouped = $derived(buildGroups(filtered));

	function getGlobalIndex(category: string, indexInGroup: number): number {
		let count = 0;
		for (const [cat, cmds] of grouped) { if (cat === category) return count + indexInGroup; count += cmds.length; }
		return 0;
	}

	function handleClose() {
		if (isClosing) return;
		isClosing = true;
		setTimeout(() => { isVisible = false; open = false; isClosing = false; query = ''; selectedIndex = 0; }, 150);
	}
	function execute(cmd: Command) { handleClose(); goto(cmd.href); }

	function handleKeyDown(event: KeyboardEvent) {
		if (!open) return;
		switch (event.key) {
			case 'Escape': event.preventDefault(); handleClose(); break;
			case 'ArrowDown': event.preventDefault(); selectedIndex = Math.min(selectedIndex + 1, filtered.length - 1); scrollToSelected(); break;
			case 'ArrowUp': event.preventDefault(); selectedIndex = Math.max(selectedIndex - 1, 0); scrollToSelected(); break;
			case 'Enter': event.preventDefault(); if (filtered[selectedIndex]) execute(filtered[selectedIndex]); break;
			case 'Home': event.preventDefault(); selectedIndex = 0; scrollToSelected(); break;
			case 'End': event.preventDefault(); selectedIndex = filtered.length - 1; scrollToSelected(); break;
		}
	}
	function scrollToSelected() {
		tick().then(() => { const el = resultsRef?.querySelector(`[data-index="${selectedIndex}"]`); if (el) el.scrollIntoView({ block: 'nearest', behavior: 'smooth' }); });
	}
	function handleBackdropClick(event: MouseEvent) { if (event.target === event.currentTarget) handleClose(); }
	function handleInput(event: Event) { query = (event.target as HTMLInputElement).value; selectedIndex = 0; }

	$effect(() => { if (open && !isVisible) tick().then(() => { isVisible = true; inputRef?.focus(); }); });
</script>

<svelte:window onkeydown={handleKeyDown} />

{#if open}
	<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
	<div class="fixed inset-0 z-50 flex items-start justify-center pt-[15vh] bg-black/50 transition-opacity duration-150" class:opacity-0={!isVisible||isClosing} class:opacity-100={isVisible&&!isClosing} onclick={handleBackdropClick} aria-hidden="true">
		<div role="dialog" aria-modal="true" tabindex="-1" aria-label="Command palette" class="w-full max-w-xl mx-4 overflow-hidden rounded-[var(--radius-md)] bg-[var(--shell-surface)] shadow-[0_4px_16px_rgba(0,0,0,0.15)] transition-all duration-150" class:scale-95={!isVisible||isClosing} class:scale-100={isVisible&&!isClosing} class:opacity-0={!isVisible||isClosing} class:opacity-100={isVisible&&!isClosing} onclick={(e)=>e.stopPropagation()}>
			<!-- Input -->
			<div class="flex items-center gap-3 px-4 py-3 border-b border-[var(--border-subtle)]">
				<Search size={18} class="shrink-0 text-[var(--shell-text-muted)]" />
				<input bind:this={inputRef} type="text" value={query} oninput={handleInput} placeholder="Type a command or search..." class="flex-1 bg-transparent text-[length:var(--text-base)] text-[var(--shell-text)] placeholder:text-[var(--shell-text-muted)] outline-none" aria-label="Command search" aria-autocomplete="list" aria-controls="command-results" aria-activedescendant={filtered.length>0?`cmd-item-${selectedIndex}`:undefined} />
				<kbd class="hidden sm:inline-flex items-center px-2 py-1 text-[length:var(--text-xs)] font-mono text-[var(--shell-text-muted)] bg-[var(--shell-surface-muted)] rounded-[var(--radius-xs)] border border-[var(--border-subtle)]">ESC</kbd>
			</div>
			<!-- Results -->
			<div bind:this={resultsRef} id="command-results" class="max-h-[50vh] overflow-y-auto" role="listbox" aria-label="Commands">
				{#if filtered.length === 0}
					<div class="px-4 py-8 text-center text-[var(--shell-text-muted)]"><p class="text-[length:var(--text-sm)]">No commands found</p></div>
				{:else}
					{#each grouped as [category, cmds]}
						<div class="px-3 py-1.5 text-[length:var(--text-xs)] font-bold uppercase tracking-wider text-[var(--shell-text-muted)] bg-[var(--shell-surface-muted)]">{category}</div>
						{#each cmds as cmd, i}
							{@const globalIndex = getGlobalIndex(category, i)}
							{@const Icon = cmd.icon}
							<button type="button" id="cmd-item-{globalIndex}" data-index={globalIndex} role="option" aria-selected={selectedIndex===globalIndex} class="w-full px-4 py-2.5 flex items-center gap-3 text-left transition-colors duration-100" class:bg-[var(--shell-accent-soft)]={selectedIndex===globalIndex} onclick={()=>execute(cmd)}>
								<Icon size={16} class="shrink-0 text-[var(--shell-text-muted)]" />
								<span class="flex-1 text-[length:var(--text-sm)] font-medium text-[var(--shell-text)]">{cmd.title}</span>
							</button>
						{/each}
					{/each}
				{/if}
			</div>
			<!-- Footer -->
			<div class="px-4 py-2 bg-[var(--shell-surface-muted)] border-t border-[var(--border-subtle)] flex items-center justify-between text-[length:var(--text-xs)] text-[var(--shell-text-muted)]">
				<div class="flex items-center gap-3">
					<span class="flex items-center gap-1"><kbd class="px-1.5 py-0.5 bg-[var(--shell-surface)] rounded-[var(--radius-xs)] border border-[var(--border-subtle)]">↑↓</kbd><span>Navigate</span></span>
					<span class="flex items-center gap-1"><kbd class="px-1.5 py-0.5 bg-[var(--shell-surface)] rounded-[var(--radius-xs)] border border-[var(--border-subtle)]">↵</kbd><span>Select</span></span>
				</div>
				<div><span>{filtered.length} command{filtered.length!==1?'s':''}</span></div>
			</div>
		</div>
	</div>
{/if}
