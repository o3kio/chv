<script lang="ts">
	import { TrendingUp, TrendingDown } from 'lucide-svelte';

	interface Props {
		label: string;
		value: string | number;
		unit?: string;
		trend?: number; // percentage
		points?: number[]; // for a small sparkline
		color?: 'primary' | 'accent' | 'success' | 'danger' | 'neutral' | 'warning';
	}

	let { label, value, unit = '', trend = 0, points = [], color = 'primary' }: Props = $props();

	function getPath(pts: number[]) {
		if (pts.length < 2) return '';
		const width = 100;
		const height = 30;
		const max = Math.max(...pts, 1);
		const min = Math.min(...pts, 0);
		const range = max - min;
		
		return pts.map((p, i) => {
			const x = (i / (pts.length - 1)) * width;
			const y = height - ((p - min) / range) * height;
			return `${i === 0 ? 'M' : 'L'} ${x} ${y}`;
		}).join(' ');
	}
</script>

<div class="compact-metric-card" style:--card-color={`var(--color-${color})`}>
	<div class="metric-top">
		<span class="metric-label">{label}</span>
		{#if trend !== 0}
			<div class="metric-trend" class:is-positive={trend > 0} class:is-negative={trend < 0}>
				{#if trend > 0}<TrendingUp size={10} />{:else}<TrendingDown size={10} />{/if}
				<span>{Math.abs(trend)}%</span>
			</div>
		{/if}
	</div>
	
	<div class="metric-main">
		<div class="value-container">
			<span class="value">{value}</span>
			<span class="unit">{unit}</span>
		</div>
		
		{#if points.length > 0}
			<div class="sparkline-container">
				<svg viewBox="0 0 100 30" preserveAspectRatio="none" class="sparkline-svg">
					<path 
						d={getPath(points)} 
						fill="none" 
						stroke="var(--card-color)" 
						stroke-width="1.5"
						stroke-linecap="round"
						stroke-linejoin="round"
					/>
				</svg>
			</div>
		{/if}
	</div>
</div>

<style>
	.compact-metric-card {
		background: var(--bg-surface);
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-xs);
		padding: 0.5rem 0.75rem;
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		min-width: 140px;
	}

	.metric-top {
		display: flex;
		justify-content: space-between;
		align-items: center;
	}

	.metric-label {
		font-size: 10px;
		font-weight: 700;
		text-transform: uppercase;
		color: var(--chv-color-neutral-500);
		letter-spacing: 0.05em;
	}

	.metric-trend {
		display: flex;
		align-items: center;
		gap: 2px;
		font-size: 9px;
		font-weight: 600;
	}

	.metric-trend.is-positive { color: var(--color-success); }
	.metric-trend.is-negative { color: var(--color-danger); }

	.metric-main {
		display: flex;
		justify-content: space-between;
		align-items: flex-end;
	}

	.value-container {
		display: flex;
		align-items: baseline;
		gap: 2px;
	}

	.value {
		font-size: var(--chv-text-lg);
		font-weight: 700;
		color: var(--chv-color-neutral-900);
		line-height: 1;
	}

	.unit {
		font-size: 10px;
		color: var(--chv-color-neutral-400);
		font-weight: 500;
	}

	.sparkline-container {
		width: 50px;
		height: 20px;
	}

	.sparkline-svg {
		width: 100%;
		height: 100%;
		overflow: visible;
	}
</style>
