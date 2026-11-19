<script lang="ts">
	import { Trash2 } from 'lucide-svelte';
	import InventoryTable from '#lib/components/shell/InventoryTable.svelte';
	import StatusBadge from '#lib/components/shell/StatusBadge.svelte';
	import ErrorState from '#lib/components/shell/ErrorState.svelte';
	import EmptyInfrastructureState from '#lib/components/shell/EmptyInfrastructureState.svelte';
	import type { ImageListItem } from '../../../routes/images/+page';

	type Column = { key: string; label: string; align?: 'left' | 'right' | 'center' };
	type ImagesModelState = 'ready' | 'empty' | 'error';
	type StatusCell = { label: string; tone: string };
	type ImageRow = Omit<ImageListItem, 'status'> & { status: StatusCell };

	interface Props {
		state: ImagesModelState;
		errorMessage?: string | null;
		columns: Column[];
		tableRows: ImageRow[];
		deletingId: string | null;
		handleDelete: (imageId: string, imageName: string, usageCount: number) => void;
	}

	let {
		state,
		errorMessage,
		columns,
		tableRows,
		deletingId,
		handleDelete
	}: Props = $props();
</script>

<section class="inventory-table-area">
	{#if state === 'error'}
		<ErrorState
			description={errorMessage ?? 'The control plane responded with an error or is unreachable.'}
		/>
	{:else if state === 'empty'}
		<EmptyInfrastructureState
			title="No artifacts detected"
			description="Adjust your search criteria or ingest a new distribution image."
			hint="Images are foundational blocks for all compute workloads."
		/>
	{:else}
		<InventoryTable
			{columns}
			rows={tableRows}
		>
			{#snippet cell({ column, row })}
				{#if column.key === '_actions'}
					<button
						type="button"
						class="btn-icon-destructive"
						disabled={deletingId === row.image_id}
						onclick={(e) => { e.preventDefault(); e.stopPropagation(); handleDelete(row.image_id, row.name, row.usage_count); }}
						title="Purge Image"
						aria-label="Purge image {row.name}"
					>
						<Trash2 size={13} />
					</button>
				{:else if column.key === 'name'}
					<div class="artifact-identity">
						<span class="artifact-name">{row.name}</span>
						{#if (row as any).is_template}
							<span class="artifact-tag">SYS</span>
						{/if}
					</div>
				{:else if (row as any)[column.key] && typeof (row as any)[column.key] === 'object' && 'label' in (row as any)[column.key]}
					<StatusBadge label={(row as any)[column.key].label} tone={(row as any)[column.key].tone} />
				{:else}
					<span class="cell-text">{(row as any)[column.key] ?? ''}</span>
				{/if}
			{/snippet}
		</InventoryTable>
	{/if}
</section>

<style>
	.artifact-identity {
		display: flex;
		align-items: center;
		gap: 0.5rem;
	}

	.artifact-name {
		font-weight: 700;
		color: var(--chv-color-neutral-900);
	}

	.artifact-tag {
		font-size: 8px;
		font-weight: 800;
		color: #ffffff;
		background: var(--chv-color-neutral-400);
		padding: 1px 3px;
		border-radius: 2px;
	}

	.btn-icon-destructive {
		background: transparent;
		border: 1px solid transparent;
		color: var(--chv-color-neutral-400);
		padding: 4px;
		border-radius: 4px;
		cursor: pointer;
		transition: all 0.1s ease;
	}

	.btn-icon-destructive:hover:not(:disabled) {
		color: var(--color-danger);
		border-color: var(--color-danger-light);
		background: var(--color-danger-light);
	}

	.btn-icon-destructive:focus-visible {
		outline: 2px solid var(--color-danger);
		outline-offset: 2px;
		color: var(--color-danger);
	}

	.cell-text {
		font-variant-numeric: tabular-nums;
	}
</style>
