<script lang="ts">
import Button from '#lib/components/primitives/Button.svelte';
	import type { VmSnapshotItem } from '#lib/bff/types.ts';
	import { listVmSnapshots, createSnapshot, deleteSnapshot, restoreSnapshot } from '#lib/bff/snapshots.ts';
	import { getStoredToken } from '#lib/api/client.ts';
	import { toast } from '#lib/stores/toast.svelte.ts';
	import { mutateWithRefresh } from '#lib/stores/mutation.svelte.ts';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import InventoryTable from '#lib/components/shell/InventoryTable.svelte';
	import EmptyInfrastructureState from '#lib/components/shell/EmptyInfrastructureState.svelte';
	import LoadingState from '#lib/components/shell/LoadingState.svelte';
	import ErrorState from '#lib/components/shell/ErrorState.svelte';
	import StatusBadge from '#lib/components/shell/StatusBadge.svelte';
	import Modal from '#lib/components/primitives/Modal.svelte';
	import ConfirmAction from '#lib/components/shared/ConfirmAction.svelte';
	import { Camera, RotateCcw, Trash2, MemoryStick, LoaderCircle } from 'lucide-svelte';
	import { formatDateTimeLabel } from '#lib/webui/formatters.ts';

	interface Props {
		vmId: string;
		snapshots?: VmSnapshotItem[];
		loading?: boolean;
		error?: string | null;
	}

	let {
		vmId,
		snapshots: propSnapshots = [],
		loading: propLoading = false,
		error: propError = null
	}: Props = $props();

	let snapshots = $state<VmSnapshotItem[]>([]);
	let localLoading = $state(false);
	let localError = $state<string | null>(null);

	let loading = $derived(propLoading || localLoading);
	let error = $derived(propError || localError);

	$effect(() => {
		snapshots = propSnapshots;
	});

	let createOpen = $state(false);
	let createName = $state('');
	let createDescription = $state('');
	let createIncludesMemory = $state(false);
	let createSubmitting = $state(false);

	let restoreOpen = $state(false);
	let restoreTarget = $state<VmSnapshotItem | null>(null);
	let restoreSubmitting = $state(false);

	let deleteOpen = $state(false);
	let deleteTarget = $state<VmSnapshotItem | null>(null);
	let deleteSubmitting = $state(false);

	const columns = [
		{ key: 'name', label: 'Name' },
		{ key: 'created_at', label: 'Created' },
		{ key: 'status', label: 'Status' },
		{ key: 'actions', label: 'Actions', align: 'right' as const }
	];

	const rows = $derived(snapshots.map(s => ({
		...s,
		created_at: formatDateTimeLabel(new Date(s.created_at).getTime()),
		status: { label: s.status, tone: normalizeTone(s.status) }
	})));

	function normalizeTone(status: string): import('#lib/shell/app-shell.ts').ShellTone {
		const s = status.toLowerCase();
		if (['ready', 'completed', 'success'].includes(s)) return 'healthy';
		if (['creating', 'pending', 'restoring'].includes(s)) return 'warning';
		if (['failed', 'error'].includes(s)) return 'failed';
		return 'unknown';
	}

	async function loadSnapshots() {
		if (!vmId) return;
		localLoading = true;
		localError = null;
		try {
			const token = getStoredToken() ?? undefined;
			const res = await listVmSnapshots({ vm_id: vmId }, token);
			snapshots = res.items;
		} catch (err) {
			localError = err instanceof Error ? err.message : 'Failed to load snapshots';
		} finally {
			localLoading = false;
		}
	}

	async function handleCreate() {
		if (!createName.trim()) {
			toast.error('Snapshot name is required');
			return;
		}
		createSubmitting = true;
		try {
			const token = getStoredToken() ?? undefined;
			await mutateWithRefresh(
				() => createSnapshot({ vm_id: vmId, name: createName.trim() }, token),
				{
					patterns: ['vms:'],
					successMessage: 'Snapshot creation accepted',
					errorMessage: 'Snapshot creation failed',
				}
			);
			createOpen = false;
			createName = '';
			createDescription = '';
			createIncludesMemory = false;
		} catch (err) {
			// Error already toasted by mutateWithRefresh
		} finally {
			createSubmitting = false;
		}
	}

	async function handleRestore() {
		if (!restoreTarget) return;
		const target = restoreTarget;
		restoreSubmitting = true;
		try {
			const token = getStoredToken() ?? undefined;
			await mutateWithRefresh(
				() => restoreSnapshot({ vm_id: vmId, snapshot_id: target.snapshot_id }, token),
				{
					patterns: ['vms:'],
					successMessage: 'Snapshot restore accepted',
					errorMessage: 'Snapshot restore failed',
				}
			);
			restoreOpen = false;
			restoreTarget = null;
		} catch (err) {
			// Error already toasted by mutateWithRefresh
		} finally {
			restoreSubmitting = false;
		}
	}

	async function handleDelete() {
		if (!deleteTarget) return;
		const target = deleteTarget;
		deleteSubmitting = true;
		try {
			const token = getStoredToken() ?? undefined;
			await mutateWithRefresh(
				() => deleteSnapshot({ vm_id: vmId, snapshot_id: target.snapshot_id }, token),
				{
					patterns: ['vms:'],
					successMessage: 'Snapshot deletion accepted',
					errorMessage: 'Snapshot deletion failed',
				}
			);
			deleteOpen = false;
			deleteTarget = null;
		} catch (err) {
			// Error already toasted by mutateWithRefresh
		} finally {
			deleteSubmitting = false;
		}
	}


</script>

<SectionCard title="VM Snapshots" icon={Camera} badgeLabel={String(snapshots.length)}>
	<div class="snapshot-header">
		<Button variant="primary" size="sm" onclick={() => { createOpen = true; }}>
			<Camera size={14} />
			Create Snapshot
		</Button>
	</div>

	{#if loading && snapshots.length === 0}
		<LoadingState title="Loading snapshots..." description="Fetching snapshot inventory from control plane." />
	{:else if error && snapshots.length === 0}
		<ErrorState title="Failed to Load Snapshots" description={error} hint="Try refreshing the page." />
	{:else if snapshots.length === 0}
		<EmptyInfrastructureState title="No Snapshots" description="This VM has no snapshots." hint="Create a snapshot to preserve the current VM state." />
	{:else}
		{#if loading}
			<div class="refresh-indicator">
				<span class="animate-spin"><LoaderCircle size={14} /></span>
				<span>Refreshing snapshots…</span>
			</div>
		{/if}
		<InventoryTable {columns} {rows}>
			{#snippet cell({ column, row })}
				{#if column.key === 'actions'}
					<div class="action-cell">
						<button
							type="button"
							class="btn-icon btn-icon-sm"
							aria-label="Restore snapshot"
							title="Restore snapshot"
							onclick={() => { restoreTarget = row as unknown as VmSnapshotItem; restoreOpen = true; }}
						>
							<RotateCcw size={14} />
						</button>
						<button
							type="button"
							class="btn-icon btn-icon-danger btn-icon-sm"
							aria-label="Delete snapshot"
							title="Delete snapshot"
							onclick={() => { deleteTarget = row as unknown as VmSnapshotItem; deleteOpen = true; }}
						>
							<Trash2 size={14} />
						</button>
					</div>
				{:else if column.key === 'name'}
					<span class="cell-name">{row.name}</span>
				{:else if column.key === 'status'}
					<StatusBadge label={row.status.label} tone={row.status.tone} />
				{:else}
					<span class="cell-text">{(row as Record<string, unknown>)[column.key]}</span>
				{/if}
			{/snippet}
		</InventoryTable>
	{/if}
</SectionCard>

<!-- Create Snapshot Modal -->
<Modal bind:open={createOpen} title="Create Snapshot">
	<div class="space-y-4">
		<div class="space-y-1">
			<label for="snap-name" class="form-label">Name</label>
			<input
				id="snap-name"
				type="text"
				bind:value={createName}
				placeholder="e.g. pre-upgrade"
				class="form-input"
				onkeydown={(e) => {
					if (e.key === 'Enter' && createName.trim() && !createSubmitting) {
						e.preventDefault();
						handleCreate();
					}
				}}
			/>
		</div>
		<div class="space-y-1">
			<label for="snap-desc" class="form-label">Description</label>
			<textarea
				id="snap-desc"
				bind:value={createDescription}
				placeholder="Optional description"
				rows={3}
				class="form-input"
			></textarea>
		</div>
		<label class="flex items-center gap-2 text-sm text-ink cursor-pointer">
			<input type="checkbox" bind:checked={createIncludesMemory} class="rounded-sm border-line" />
			<MemoryStick size={14} />
			Include memory state
		</label>
	</div>
	{#snippet footer()}
		<Button type="button" variant="secondary" size="sm" onclick={() => { createOpen = false; }}>Cancel</Button>
		<Button
			type="button"
			variant="primary"
			size="sm"
			disabled={createSubmitting || !createName.trim()}
			onclick={handleCreate}
		>
			{#if createSubmitting}
				<span class="animate-spin"><LoaderCircle size={14} /></span>
			{/if}
			Create Snapshot
		</Button>
	{/snippet}
</Modal>

<!-- Restore Confirm -->
<ConfirmAction
	bind:open={restoreOpen}
	title="Restore Snapshot"
	description="VM must be stopped to restore a snapshot. The restore operation will revert the VM to the selected snapshot state."
	actionType="restore"
	confirmText="Restore"
	onConfirm={handleRestore}
/>

<!-- Delete Confirm -->
<ConfirmAction
	bind:open={deleteOpen}
	title="Delete Snapshot"
	description="This snapshot will be permanently deleted. This action cannot be undone."
	actionType="delete"
	confirmText="Delete"
	onConfirm={handleDelete}
/>

<style>
	.snapshot-header {
		display: flex;
		justify-content: flex-end;
		margin-bottom: 0.75rem;
	}

	.action-cell {
		display: flex;
		justify-content: flex-end;
		gap: 0.35rem;
	}

	.btn-icon {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 1.75rem;
		height: 1.75rem;
		border-radius: 0.3rem;
		border: 1px solid var(--shell-line);
		background: white;
		color: var(--shell-text-secondary);
		cursor: pointer;
		transition: all 0.15s;
	}

	.btn-icon:hover {
		background: var(--shell-surface-muted);
		color: var(--shell-text);
	}

	.btn-icon-danger:hover {
		border-color: var(--color-danger);
		color: var(--color-danger);
		background: var(--color-danger-light);
	}

	.btn-icon-sm {
		width: 1.5rem;
		height: 1.5rem;
	}

	.form-label {
		display: block;
		font-size: var(--chv-text-sm);
		font-weight: 500;
		color: var(--shell-text);
	}

	.form-input {
		width: 100%;
		padding: 0.5rem 0.75rem;
		font-size: var(--chv-text-sm);
		color: var(--shell-text);
		background: white;
		border: 1px solid var(--shell-line);
		border-radius: 0.35rem;
		outline: none;
		transition: border-color 0.15s, box-shadow 0.15s;
	}

	.form-input:focus {
		border-color: var(--shell-accent);
		box-shadow: 0 0 0 2px var(--shell-accent-soft);
	}

	.cell-name {
		font-weight: 600;
		color: var(--shell-text);
	}

	.cell-text {
		color: var(--shell-text-secondary);
	}

	.space-y-1 {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
	}

	.space-y-4 {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.flex {
		display: flex;
	}

	.items-center {
		align-items: center;
	}

	.gap-2 {
		gap: 0.5rem;
	}

	.cursor-pointer {
		cursor: pointer;
	}

	.text-sm {
		font-size: var(--chv-text-sm);
	}

	.text-ink {
		color: var(--shell-text);
	}

	.border-line {
		border-color: var(--shell-line);
	}

	.refresh-indicator {
		display: flex;
		align-items: center;
		gap: 0.4rem;
		font-size: var(--chv-text-xs);
		color: var(--shell-text-muted);
		padding: 0.35rem 0.5rem;
		margin-bottom: 0.5rem;
		background: var(--shell-surface-muted);
		border-radius: 0.25rem;
		width: fit-content;
	}

	.animate-spin {
		animation: spin 1s linear infinite;
	}

	@keyframes spin {
		from { transform: rotate(0deg); }
		to { transform: rotate(360deg); }
	}
</style>
