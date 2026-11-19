<script lang="ts">
	import type { DriftStatus } from '#lib/bff/architectures.ts';

	interface Props {
		status: DriftStatus;
		/** When true, render a smaller pill (used inside dashboard cards). */
		compact?: boolean;
	}

	let { status, compact = false }: Props = $props();

	const labels: Record<DriftStatus, string> = {
		unknown: 'Drift unknown',
		no_drift: 'No drift',
		drifted: 'Drifted',
		check_failed: 'Check failed'
	};

	// Phase-6 status colors: green / amber / red / neutral. Tokens come from
	// global CSS variables — no Tailwind classes here so the badge stays
	// usable in non-Tailwind contexts (panel header, dashboard card).
	const statusClass = $derived(`status-${status}`);
	const ariaLabel = $derived(`Drift status: ${labels[status]}`);
</script>

<span
	class="badge {statusClass}"
	class:compact
	role="status"
	aria-label={ariaLabel}
	data-testid="drift-status-badge"
	data-drift-status={status}
>
	{labels[status]}
</span>

<style>
	.badge {
		display: inline-block;
		padding: 0.15rem 0.5rem;
		font-size: 11px;
		font-weight: 700;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		border-radius: var(--radius-xs);
		color: white;
		white-space: nowrap;
	}

	.badge.compact {
		font-size: 10px;
		padding: 0.1rem 0.4rem;
	}

	.status-unknown {
		background: var(--chv-color-neutral-500, #6b7280);
	}

	.status-no_drift {
		background: #15803d;
	}

	.status-drifted {
		background: #b45309;
	}

	.status-check_failed {
		background: #b91c1c;
	}

	@media (prefers-contrast: more) {
		.badge {
			outline: 1px solid currentColor;
			outline-offset: -1px;
		}
	}
</style>
