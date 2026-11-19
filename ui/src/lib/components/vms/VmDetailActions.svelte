<script lang="ts">
	import Button from '#lib/components/primitives/Button.svelte';
	import { Play, Square, RotateCcw, Trash2, Power, ArrowRightLeft } from 'lucide-svelte';

	interface Props {
		pendingAction?: string | null;
		powerState?: string;
		onExecute?: (action: string) => void;
		onMigrate?: () => void;
	}

	let {
		pendingAction = null,
		powerState = '',
		onExecute = () => {},
		onMigrate
	}: Props = $props();

	let confirmingAction = $state<string | null>(null);

	const ps = $derived(powerState.toLowerCase());
</script>

<div class="header-actions">
	{#if confirmingAction}
		<div class="confirm-group">
			<span class="confirm-text">Confirm {confirmingAction}?</span>
			<Button variant="primary" size="sm" onclick={() => { onExecute(confirmingAction!); confirmingAction = null; }}>Confirm</Button>
			<Button variant="secondary" size="sm" onclick={() => confirmingAction = null}>Cancel</Button>
		</div>
	{:else}
		<button class="vm-action vm-action--primary" type="button" disabled={ps === 'running' || pendingAction !== null} onclick={() => onExecute('start')} title={pendingAction === 'start' ? 'Starting' : 'Start VM'} aria-label={pendingAction === 'start' ? 'Starting VM' : 'Start VM'}>
			<Play size={13} />
		</button>
		<button class="vm-action" type="button" disabled={ps !== 'running' || pendingAction !== null} onclick={() => confirmingAction = 'shutdown'} title={pendingAction === 'shutdown' ? 'Shutting down' : 'Shutdown VM'} aria-label={pendingAction === 'shutdown' ? 'Shutting down VM' : 'Shutdown VM'}>
			<Power size={13} />
		</button>
		<button class="vm-action" type="button" disabled={ps !== 'running' || pendingAction !== null} onclick={() => confirmingAction = 'poweroff'} title={pendingAction === 'poweroff' ? 'Powering off' : 'Poweroff VM'} aria-label={pendingAction === 'poweroff' ? 'Powering off VM' : 'Poweroff VM'}>
			<Square size={13} />
		</button>
		<button class="vm-action" type="button" disabled={ps !== 'running' || pendingAction !== null} onclick={() => confirmingAction = 'restart'} title={pendingAction === 'restart' ? 'Rebooting' : 'Reboot VM'} aria-label={pendingAction === 'restart' ? 'Rebooting VM' : 'Reboot VM'}>
			<RotateCcw size={13} />
		</button>
		<button class="vm-action" type="button" disabled={ps !== 'running' || pendingAction !== null} onclick={() => onMigrate?.()} title="Migrate VM" aria-label="Migrate VM to another node">
			<ArrowRightLeft size={13} />
		</button>
		<button class="vm-action vm-action--danger" type="button" disabled={pendingAction !== null} onclick={() => confirmingAction = 'delete'} title={pendingAction === 'delete' ? 'Deleting' : 'Delete VM'} aria-label={pendingAction === 'delete' ? 'Deleting VM' : 'Delete VM'}>
			<Trash2 size={13} />
		</button>
	{/if}
</div>

<style>
	.header-actions {
		display: flex;
		flex-wrap: wrap;
		gap: 0.25rem;
		align-items: center;
		justify-content: flex-end;
	}

	.vm-action {
		display: inline-grid;
		place-items: center;
		width: 1.85rem;
		height: 1.85rem;
		padding: 0;
		border: 1px solid var(--shell-line);
		border-radius: var(--radius-xs);
		background: var(--shell-surface);
		color: var(--shell-text-secondary);
		cursor: pointer;
		transition:
			background 120ms ease,
			border-color 120ms ease,
			color 120ms ease;
	}

	.vm-action:hover:not(:disabled) {
		border-color: var(--shell-accent);
		background: var(--shell-accent-soft);
		color: var(--shell-text);
	}

	.vm-action:disabled {
		cursor: not-allowed;
		opacity: 0.42;
	}

	.vm-action--primary {
		background: var(--shell-accent);
		border-color: var(--shell-accent);
		color: var(--color-sidebar-text-active, #ffffff);
	}

	.vm-action--primary:hover:not(:disabled) {
		background: var(--color-primary-active);
		color: var(--color-sidebar-text-active, #ffffff);
	}

	.vm-action--danger:hover:not(:disabled) {
		border-color: var(--color-danger);
		background: var(--color-danger-light);
		color: var(--color-danger-dark);
	}

	.confirm-group {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: 0.5rem;
		background: var(--color-danger-light);
		padding: 0.45rem 0.65rem;
		border-radius: var(--radius-sm);
		border: 1px solid var(--color-danger);
	}

	.confirm-text {
		font-size: var(--chv-text-sm);
		color: var(--color-danger-dark);
		font-weight: 600;
	}

	@media (max-width: 720px) {
		.confirm-group {
			align-items: stretch;
		}
		.header-actions :global(button),
		.confirm-group :global(button) {
			flex: 0 0 auto;
		}
	}
</style>
