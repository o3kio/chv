<script lang="ts" generics="T extends Record<string, unknown>">
	import type { Snippet } from 'svelte';
	import StatusBadge from './StatusBadge.svelte';
	import type { ShellTone } from '#lib/shell/app-shell.ts';

	interface Column {
		key: string;
		label: string;
		align?: 'left' | 'right' | 'center';
	}

	interface BadgeData {
		label: string;
		tone: ShellTone;
	}

	interface Props {
		columns: Column[];
		rows: T[];
		rowHref?: (row: T) => string | null;
		emptySnippet?: Snippet;
		cell?: Snippet<[{ column: Column, row: T }]>
	}

	let { columns, rows, rowHref, emptySnippet, cell }: Props = $props();

	function isBadge(val: unknown): val is BadgeData {
		return val !== null && typeof val === 'object' && 'tone' in val && 'label' in val;
	}
</script>

<div class="w-full overflow-x-auto border border-(--shell-line) rounded-[0.35rem] bg-(--shell-surface)">
	{#if rows.length === 0}
		{#if emptySnippet}
			{@render emptySnippet()}
		{:else}
			<div class="p-8 text-center text-(--shell-text-muted) text-(length:--text-sm)" role="status">No inventory matched current filters.</div>
		{/if}
	{:else}
		<table class="w-full border-collapse text-left">
			<thead>
				<tr>
					{#each columns as col}
						<th class="bg-(--shell-surface-muted) px-3 py-[0.45rem] text-(length:--text-xs) font-semibold uppercase tracking-wider text-(--shell-text-muted) border-b border-(--shell-line) whitespace-nowrap {col.align === 'right' ? 'text-right' : col.align === 'center' ? 'text-center' : 'text-left'}" scope="col">{col.label}</th>
					{/each}
				</tr>
			</thead>
			<tbody>
				{#each rows as row, rowIndex}
					<tr class="hover:bg-[rgba(143,90,42,0.02)] {rowIndex === rows.length - 1 ? "[&_td]:border-b-0" : ""}">
						{#each columns as col, i}
							{@const val = (row as Record<string, unknown>)[col.key]}
							<td class="px-3 py-[0.4rem] border-b border-(--shell-line) text-(length:--text-sm) text-(--shell-text-secondary) align-middle whitespace-nowrap {col.align === 'right' ? 'text-right' : col.align === 'center' ? 'text-center' : 'text-left'}">
								{#if cell}
									{@render cell({ column: col, row })}
								{:else if i === 0 && rowHref?.(row)}
									<a href={rowHref(row)} class="text-(--shell-text) font-semibold no-underline hover:text-(--shell-accent) hover:underline">
										{val}
									</a>
								{:else if isBadge(val)}
									<StatusBadge label={val.label} tone={val.tone} />
								{:else}
									<span class="tabular-nums">{val}</span>
								{/if}
							</td>
						{/each}
					</tr>
				{/each}
			</tbody>
		</table>
	{/if}
</div>
