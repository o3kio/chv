<script lang="ts">
	import type { InstanceActionDefinition } from '#lib/api/types.ts';
	import {
		ExternalLink,
		Terminal,
		Play,
		Power,
		Zap,
		RotateCcw,
		Pencil,
		Trash2,
		MoreVertical
	} from 'lucide-svelte';

	interface Props {
		actions: InstanceActionDefinition[];
		onAction: (actionId: string) => void;
		instanceName: string;
	}

	let { actions, onAction, instanceName }: Props = $props();

	let menuOpen = $state(false);
	let menuPos = $state({ x: 0, y: 0 });
	let menuElement = $state<HTMLDivElement | null>(null);

	const iconMap: Record<string, typeof ExternalLink> = {
		open: ExternalLink,
		console: Terminal,
		start: Play,
		shutdown: Power,
		poweroff: Zap,
		restart: RotateCcw,
		rename: Pencil,
		delete: Trash2
	};

	export function openAt(x: number, y: number) {
		menuPos = { x, y };
		menuOpen = true;
	}

	export function close() {
		menuOpen = false;
	}

	export function toggleAt(x: number, y: number) {
		if (menuOpen) {
			close();
		} else {
			openAt(x, y);
		}
	}

	function handleActionClick(actionId: string) {
		close();
		onAction(actionId);
	}

	function handleKeyDown(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			close();
		}
	}

	function handleClickOutside(event: MouseEvent) {
		if (menuElement && !menuElement.contains(event.target as Node)) {
			close();
		}
	}

	$effect(() => {
		if (menuOpen) {
			document.addEventListener('click', handleClickOutside, { capture: true });
			document.addEventListener('keydown', handleKeyDown, { capture: true });
			// Adjust position if menu would overflow viewport
			requestAnimationFrame(() => {
				if (!menuElement) return;
				const rect = menuElement.getBoundingClientRect();
				const vw = window.innerWidth;
				const vh = window.innerHeight;
				let nx = menuPos.x;
				let ny = menuPos.y;
				if (nx + rect.width > vw) nx = vw - rect.width - 8;
				if (ny + rect.height > vh) ny = vh - rect.height - 8;
				if (nx < 8) nx = 8;
				if (ny < 8) ny = 8;
				if (nx !== menuPos.x || ny !== menuPos.y) {
					menuPos = { x: nx, y: ny };
				}
			});
		} else {
			document.removeEventListener('click', handleClickOutside, { capture: true });
			document.removeEventListener('keydown', handleKeyDown, { capture: true });
		}
		return () => {
			document.removeEventListener('click', handleClickOutside, { capture: true });
			document.removeEventListener('keydown', handleKeyDown, { capture: true });
		};
	});
</script>

{#if menuOpen}
	<div
		bind:this={menuElement}
		class="fixed z-50 min-w-48 rounded-md border border-(--shell-line) bg-(--bg-sidebar) shadow-lg py-1"
		style="left: {menuPos.x}px; top: {menuPos.y}px;"
		role="menu"
		aria-label="Actions for instance {instanceName}"
	>
		{#each actions as action, i (action.id)}
			{#if action.dangerous && i > 0 && !actions[i - 1].dangerous}
				<div class="my-1 border-t border-(--shell-line)" role="separator"></div>
			{/if}
			{@const Icon = iconMap[action.id] ?? ExternalLink}
			<button
				type="button"
				role="menuitem"
				disabled={!action.enabled}
				class="w-full flex items-center gap-2.5 px-3 py-1.5 text-left text-(length:--text-sm) transition-colors
					{action.enabled
						? action.dangerous
							? 'text-danger hover:bg-(--color-danger-light)'
							: 'text-(--color-sidebar-text) hover:bg-neutral-800'
						: 'text-neutral-600 cursor-not-allowed'}
				"
				onclick={() => handleActionClick(action.id)}
				title={action.disabledReason ?? action.label}
				aria-disabled={!action.enabled}
			>
				<Icon size={14} aria-hidden="true" />
				<span class="flex-1">{action.label}</span>
				{#if !action.enabled && action.disabledReason}
					<span class="text-(length:--text-xs) text-neutral-600 truncate max-w-24">
						{action.disabledReason}
					</span>
				{/if}
			</button>
		{/each}
	</div>
{/if}


