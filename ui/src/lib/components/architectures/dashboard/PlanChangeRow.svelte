<script lang="ts">
	import type { PlanAction, PlanChange, PlanRisk } from '#lib/bff/architectures.ts';

	interface Props {
		change: PlanChange;
	}

	let { change }: Props = $props();

	const actionLabel: Record<PlanAction, string> = {
		create: 'Create',
		update: 'Update',
		delete: 'Delete',
		replace: 'Replace',
		no_op: 'No-op'
	};

	const riskLabel: Record<PlanRisk, string> = {
		low: 'low',
		medium: 'medium',
		high: 'high',
		destructive: 'destructive'
	};
</script>

<li class="cr" data-testid="plan-change-row">
	<span
		class="ac risk-{change.risk}"
		data-testid="plan-change-action"
		aria-label={`${actionLabel[change.action]} (risk: ${riskLabel[change.risk]})`}
	>{actionLabel[change.action]}</span>
	<span class="ref" data-testid="plan-change-ref">{change.resource_ref}</span>
	<span class="dsc" data-testid="plan-change-desc">{change.description}</span>
</li>

<style>
	.cr {
		display: grid;
		grid-template-columns: auto minmax(120px, 1fr) minmax(0, 2fr);
		gap: 0.5rem;
		align-items: baseline;
		padding: 0.4rem 0.55rem;
		background: var(--chv-color-neutral-50, #f8fafc);
		border: 1px solid var(--chv-color-neutral-200);
		border-radius: var(--radius-xs);
		font-size: 12px;
		color: inherit;
	}
	.ac {
		display: inline-block;
		padding: 0.1rem 0.45rem;
		font-size: 11px;
		font-weight: 700;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		border-radius: var(--radius-xs);
		color: white;
		min-width: 60px;
		text-align: center;
	}
	.risk-low { background: #15803d; }
	.risk-medium { background: #b45309; }
	.risk-high { background: #c2410c; }
	.risk-destructive { background: #b91c1c; }
	.ref {
		font-family: var(--chv-font-mono, ui-monospace, SFMono-Regular, monospace);
		color: var(--chv-color-neutral-700);
	}
	.dsc { color: var(--chv-color-neutral-600); }
</style>
