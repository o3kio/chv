<script lang="ts">
	import { DRIFT_FINDING_CODES, type DriftFindingCode, type DriftSummary } from '#lib/bff/architectures.ts';

	interface Props {
		summary: DriftSummary;
	}

	let { summary }: Props = $props();

	// Short labels for the chip strip — kept compact so all 7 fit on one line
	// at desktop widths. Aligned 1:1 with the Rust `code` field in
	// `crates/chv-architecture-reconcile/src/drift.rs`.
	const labels: Record<DriftFindingCode, string> = {
		DRIFT_MISSING_RESOURCE: 'Missing',
		DRIFT_UNEXPECTED_RESOURCE: 'Unexpected',
		DRIFT_FIELD_CHANGED: 'Field',
		DRIFT_CAPACITY_CHANGED: 'Capacity',
		DRIFT_NETWORK_CHANGED: 'Network',
		DRIFT_PERMISSION_CHANGED: 'Permission',
		DRIFT_ATTACHMENT_CHANGED: 'Attachment'
	};

	// Iterate the canonical code list rather than `Object.keys(by_type)` so
	// chips render in stable order even when the server omits zero entries.
	const chips = $derived(
		DRIFT_FINDING_CODES.map((code) => ({
			code,
			label: labels[code],
			count: summary.by_type[code] ?? 0
		}))
	);
</script>

<div class="chips" aria-label="Drift findings by type">
	{#each chips as chip (chip.code)}
		<span
			class="chip"
			class:chip-zero={chip.count === 0}
			class:chip-active={chip.count > 0}
			aria-label={`${chip.label}: ${chip.count}`}
			data-testid="drift-summary-chip"
			data-drift-chip-code={chip.code}
		>
			<span class="chip-label">{chip.label}</span>
			<span class="chip-count">{chip.count}</span>
		</span>
	{/each}
</div>

<style>
	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 0.4rem;
	}

	.chip {
		display: inline-flex;
		align-items: center;
		gap: 0.35rem;
		padding: 0.15rem 0.5rem 0.15rem 0.55rem;
		font-size: 11px;
		font-weight: 600;
		border-radius: var(--radius-xs);
		background: var(--chv-color-neutral-50, #f8fafc);
		border: 1px solid var(--chv-color-neutral-200);
		color: var(--chv-color-neutral-600);
	}

	.chip-active {
		background: rgba(180, 83, 9, 0.1);
		border-color: rgba(180, 83, 9, 0.35);
		color: rgb(120, 53, 15);
	}

	.chip-zero {
		opacity: 0.7;
	}

	.chip-count {
		font-variant-numeric: tabular-nums;
		font-weight: 700;
	}
</style>
