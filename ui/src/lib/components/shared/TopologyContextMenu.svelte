<script lang="ts">
	import { fade } from 'svelte/transition';

	interface MenuAction {
		label: string;
		hint?: string;
		dangerous?: boolean;
		disabled?: boolean;
		run: () => void;
	}

	interface Props {
		contextMenu: {
			x: number;
			y: number;
			title: string;
			subtitle: string;
			type: string;
			actions: MenuAction[];
		} | null;
		onClose: () => void;
	}

	let { contextMenu, onClose }: Props = $props();
	let menuElement = $state<HTMLDivElement | null>(null);
	let clampedX = $state(0);
	let clampedY = $state(0);

	$effect(() => {
		if (!contextMenu) return;
		clampedX = contextMenu.x;
		clampedY = contextMenu.y;
		requestAnimationFrame(() => {
			if (!menuElement) return;
			const rect = menuElement.getBoundingClientRect();
			clampedX = Math.min(Math.max(contextMenu.x, 8), window.innerWidth - rect.width - 8);
			clampedY = Math.min(Math.max(contextMenu.y, 8), window.innerHeight - rect.height - 8);
		});
	});

	$effect(() => {
		if (!contextMenu) return;

		function handleDocumentClick(event: MouseEvent) {
			if (menuElement?.contains(event.target as Node)) return;
			onClose();
		}

		function handleDocumentKeydown(event: KeyboardEvent) {
			if (event.key === 'Escape') onClose();
		}

		document.addEventListener('click', handleDocumentClick, { capture: true });
		document.addEventListener('keydown', handleDocumentKeydown, { capture: true });
		return () => {
			document.removeEventListener('click', handleDocumentClick, { capture: true });
			document.removeEventListener('keydown', handleDocumentKeydown, { capture: true });
		};
	});

	function runAction(action: MenuAction) {
		if (action.disabled) return;
		onClose();
		action.run();
	}
</script>

{#if contextMenu}
	<div
		bind:this={menuElement}
		class="topology-menu"
		class:topology-menu--danger={contextMenu.type === 'vm'}
		style:left={`${clampedX}px`}
		style:top={`${clampedY}px`}
		role="menu"
		aria-label="Topology actions for {contextMenu.title}"
		transition:fade={{ duration: 80 }}
	>
		<div class="topology-menu__header">
			<strong>{contextMenu.title}</strong>
			<span>{contextMenu.subtitle}</span>
		</div>
		<div class="topology-menu__items">
			{#each contextMenu.actions as action}
				<button
					type="button"
					role="menuitem"
					disabled={action.disabled}
					class:topology-menu__item--danger={action.dangerous}
					onclick={() => runAction(action)}
				>
					<span>{action.label}</span>
					{#if action.hint}<small>{action.hint}</small>{/if}
				</button>
			{/each}
		</div>
	</div>
{/if}

<style>
	.topology-menu {
		position: fixed;
		z-index: 60;
		width: 14rem;
		border: 1px solid var(--shell-line-strong);
		border-radius: var(--radius-sm);
		background: var(--shell-surface);
		box-shadow: var(--chv-shadow-lg);
		overflow: hidden;
	}

	.topology-menu__header {
		display: flex;
		flex-direction: column;
		gap: 0.1rem;
		padding: 0.65rem 0.75rem;
		border-bottom: 1px solid var(--shell-line);
		background: var(--shell-surface-muted);
	}

	.topology-menu__header strong {
		font-size: var(--chv-text-sm);
		color: var(--shell-text);
	}

	.topology-menu__header span {
		font-size: var(--chv-text-xs);
		color: var(--shell-text-muted);
	}

	.topology-menu__items {
		padding: 0.25rem;
	}

	.topology-menu__items button {
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto;
		align-items: center;
		width: 100%;
		gap: 0.5rem;
		padding: 0.5rem;
		border: 0;
		border-radius: var(--radius-xs);
		background: transparent;
		color: var(--shell-text);
		cursor: pointer;
		text-align: left;
	}

	.topology-menu__items button:hover:not(:disabled),
	.topology-menu__items button:focus-visible {
		background: var(--shell-surface-muted);
		outline: none;
	}

	.topology-menu__items button:disabled {
		cursor: not-allowed;
		color: var(--chv-color-neutral-400);
	}

	.topology-menu__items small {
		font-size: 9px;
		color: var(--shell-text-muted);
		text-transform: uppercase;
		letter-spacing: 0.05em;
	}
</style>
