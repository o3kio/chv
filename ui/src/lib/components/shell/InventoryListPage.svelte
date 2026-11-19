<script lang="ts" generics="T extends Record<string, unknown>">
	import type { Snippet } from 'svelte';
	import PageHeaderWithAction from './PageHeaderWithAction.svelte';
	import InventoryTable from './InventoryTable.svelte';
	import FilterBar from '#lib/components/shared/FilterBar.svelte';
	import ErrorState from './ErrorState.svelte';
	import EmptyInfrastructureState from './EmptyInfrastructureState.svelte';
	import SectionCard from './SectionCard.svelte';
	import CompactMetricCard from '#lib/components/shared/CompactMetricCard.svelte';
	import type { PageDefinition } from '#lib/shell/app-shell.ts';

	interface FilterDef {
		key: string;
		label: string;
		type: 'text' | 'select' | 'boolean';
		placeholder?: string;
		options?: { value: string; label: string }[];
	}

	interface ColumnDef<T = unknown> {
		key: string;
		label: string;
		align?: 'left' | 'right' | 'center';
	}

	interface Props {
		page: PageDefinition;
		headerActions?: Snippet;
		metrics: { label: string; value: number; trend?: number; color?: string }[];
		filters: FilterDef[];
		activeFilters: Record<string, string>;
		onFilterChange: (key: string, value: unknown) => void;
		onClearFilters: () => void;
		state: 'ready' | 'empty' | 'error';
		emptyTitle: string;
		emptyDescription: string;
		emptyHint: string;
		columns: ColumnDef<T>[];
		rows: T[];
		rowHref: (row: T) => string;
		cell?: Snippet<[{ column: ColumnDef<T>, row: T }]>;
		sidebar: Snippet;
	}

	let {
		page,
		headerActions,
		metrics,
		filters,
		activeFilters,
		onFilterChange,
		onClearFilters,
		state,
		emptyTitle,
		emptyDescription,
		emptyHint,
		columns,
		rows,
		rowHref,
		cell,
		sidebar
	}: Props = $props();
</script>

<div class="flex flex-col gap-3">
	<PageHeaderWithAction {page}>
		{#snippet actions()}
			{#if headerActions}
				{@render headerActions()}
			{/if}
		{/snippet}
	</PageHeaderWithAction>

	<div class="grid gap-3" style="grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));">
		{#each metrics as m}
			<CompactMetricCard
				label={m.label}
				value={m.value}
				trend={m.trend ?? 0}
				color={m.color as 'primary' | 'accent' | 'success' | 'danger' | 'neutral' | 'warning' | undefined}
			/>
		{/each}
	</div>

	<div class="bg-(--bg-surface) border border-(--border-subtle) rounded-xs overflow-hidden">
		<FilterBar
			{filters}
			{activeFilters}
			onFilterChange={onFilterChange}
			onClearAll={onClearFilters}
		/>
	</div>

	<main class="grid gap-4 items-start grid-cols-[1fr_300px] max-[1100px]:grid-cols-1!">
		<section>
			{#if state === 'error'}
				<ErrorState />
			{:else if state === 'empty'}
				<EmptyInfrastructureState
					title={emptyTitle}
					description={emptyDescription}
					hint={emptyHint}
				/>
			{:else}
				<InventoryTable {columns} {rows} {rowHref} {cell} />
			{/if}
		</section>

		<aside class="flex flex-col gap-4">
			{@render sidebar()}
		</aside>
	</main>
</div>
