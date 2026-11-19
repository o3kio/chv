<script lang="ts">
	import type { DriftFinding } from '#lib/bff/architectures.ts';

	interface Props {
		finding: DriftFinding;
	}

	let { finding }: Props = $props();

	// Compact human label per code — mirrors DriftSummaryChips so the row
	// header reads consistently with the chip strip above the list.
	const codeLabels: Record<DriftFinding['code'], string> = {
		DRIFT_MISSING_RESOURCE: 'Missing resource',
		DRIFT_UNEXPECTED_RESOURCE: 'Unexpected resource',
		DRIFT_FIELD_CHANGED: 'Field changed',
		DRIFT_CAPACITY_CHANGED: 'Capacity changed',
		DRIFT_NETWORK_CHANGED: 'Network changed',
		DRIFT_PERMISSION_CHANGED: 'Permission changed',
		DRIFT_ATTACHMENT_CHANGED: 'Attachment changed'
	};

	// `null` rendering for NetworkChanged (e.g. no bridge configured before).
	function renderNullable(value: string | null): string {
		return value === null ? '∅' : value;
	}
</script>

<li
	class="row"
	data-testid="drift-finding-row"
	data-drift-code={finding.code}
	aria-label={`${codeLabels[finding.code]}: ${finding.resource_ref}`}
>
	<div class="row-header">
		<span class="row-code" aria-label="Finding code">
			{codeLabels[finding.code]}
		</span>
		<span class="row-ref" data-testid="drift-finding-ref">{finding.resource_ref}</span>
	</div>
	<div class="row-message">{finding.message}</div>
	<div class="row-meta">
		<span class="row-path" aria-label="Path">{finding.path}</span>
	</div>

	{#if finding.code === 'DRIFT_FIELD_CHANGED'}
		<div class="diff" aria-label="Field change">
			<span class="diff-field">{finding.field}</span>
			<span class="diff-arrow" aria-hidden="true">→</span>
			<span class="diff-expected">{finding.expected}</span>
			<span class="diff-arrow" aria-hidden="true">↦</span>
			<span class="diff-actual">{finding.actual}</span>
		</div>
	{:else if finding.code === 'DRIFT_CAPACITY_CHANGED'}
		<div class="diff" aria-label="Capacity change">
			<span class="diff-field">{finding.field}</span>
			<span class="diff-arrow" aria-hidden="true">→</span>
			<span class="diff-expected">{finding.expected}</span>
			<span class="diff-arrow" aria-hidden="true">↦</span>
			<span class="diff-actual">{finding.actual}</span>
		</div>
	{:else if finding.code === 'DRIFT_NETWORK_CHANGED'}
		<div class="diff" aria-label="Network change">
			<span class="diff-field">{finding.field}</span>
			<span class="diff-arrow" aria-hidden="true">→</span>
			<span class="diff-expected">{renderNullable(finding.expected)}</span>
			<span class="diff-arrow" aria-hidden="true">↦</span>
			<span class="diff-actual">{renderNullable(finding.actual)}</span>
		</div>
	{/if}
</li>

<style>
	.row {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		padding: 0.6rem 0.75rem;
		background: var(--bg-surface);
		border: 1px solid var(--chv-color-neutral-200);
		border-radius: var(--radius-xs);
	}

	.row-header {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		gap: 0.5rem;
		flex-wrap: wrap;
	}

	.row-code {
		font-size: 11px;
		font-weight: 700;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: rgb(120, 53, 15);
	}

	.row-ref {
		font-family: var(--chv-font-mono, ui-monospace, monospace);
		font-size: 12px;
		color: var(--chv-color-neutral-700);
	}

	.row-message {
		font-size: var(--chv-text-sm);
		color: var(--chv-color-neutral-800);
		line-height: 1.4;
	}

	.row-meta {
		display: flex;
		gap: 0.5rem;
		font-size: 11px;
		color: var(--chv-color-neutral-500);
	}

	.row-path {
		font-family: var(--chv-font-mono, ui-monospace, monospace);
	}

	.diff {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.35rem;
		padding: 0.3rem 0.5rem;
		background: var(--chv-color-neutral-50, #f8fafc);
		border: 1px dashed var(--chv-color-neutral-200);
		border-radius: var(--radius-xs);
		font-family: var(--chv-font-mono, ui-monospace, monospace);
		font-size: 11px;
		color: var(--chv-color-neutral-700);
	}

	.diff-field {
		font-weight: 700;
		color: var(--chv-color-neutral-800);
	}

	.diff-arrow {
		color: var(--chv-color-neutral-400);
	}

	.diff-expected {
		color: rgb(6, 95, 70);
	}

	.diff-actual {
		color: rgb(153, 27, 27);
	}
</style>
