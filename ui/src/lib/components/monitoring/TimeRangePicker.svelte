<script lang="ts">
	import { MONITORING_TIME_RANGES, type MonitoringTimeRange } from '#lib/bff/monitoring.ts';

	interface Props {
		value: MonitoringTimeRange;
		onChange: (range: MonitoringTimeRange) => void;
	}

	let { value, onChange }: Props = $props();
</script>

<div class="range-picker" role="group" aria-label="Time range">
	{#each MONITORING_TIME_RANGES as range (range)}
		<button
			type="button"
			class="range-button"
			class:active={value === range}
			aria-pressed={value === range}
			onclick={() => onChange(range)}
		>
			{range}
		</button>
	{/each}
</div>

<style>
	.range-picker {
		display: inline-flex;
		gap: 0.25rem;
		padding: 0.25rem;
		background: var(--bg-surface-muted);
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-xs);
	}

	.range-button {
		padding: 0.25rem 0.6rem;
		font-size: 10px;
		font-weight: 700;
		font-family: var(--chv-font-mono);
		color: var(--chv-color-neutral-500);
		background: transparent;
		border: none;
		border-radius: var(--radius-xs);
		cursor: pointer;
	}

	.range-button:hover {
		color: var(--chv-color-neutral-900);
	}

	.range-button.active {
		color: var(--color-primary);
		background: var(--bg-surface);
	}
</style>
