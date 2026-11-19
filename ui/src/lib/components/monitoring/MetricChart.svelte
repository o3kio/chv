<script lang="ts">
	import type { ChartData } from 'chart.js';
	import ChartJS from '#lib/components/shared/charts/ChartJS.svelte';
	import type { MonitoringSeries } from '#lib/bff/monitoring.ts';
	import {
		formatCounterRate,
		formatAge,
		metricTitle,
		qualityLabel,
		reasonLabel,
		sourceLabel
	} from '#lib/monitoring/format.ts';

	interface Props {
		series: MonitoringSeries;
		nowMs: number;
	}

	let { series, nowMs }: Props = $props();

	let title = $derived(
		series.dimensions['interface_id'] || series.dimensions['block_device_id']
			? `${metricTitle(series.metric_id)} (${series.dimensions['interface_id'] ?? series.dimensions['block_device_id']})`
			: metricTitle(series.metric_id)
	);

	// Chart values: gauges render their value; counters render the rate
	// over each point's own window (delta / window). Non-valid points
	// are gaps (null) — never interpolated, never zero.
	let chartData = $derived<ChartData<'line'>>({
		labels: series.points.map((p) => new Date(p.timestamp_ms).toLocaleTimeString()),
		datasets: [
			{
				label: title,
				data: series.points.map((p) => {
					if (p.quality !== 'valid') return null;
					if (series.kind === 'counter') {
						return formatCounterRate(p.integer_value, p.window_ms);
					}
					return p.value ?? null;
				}),
				borderColor: '#0f62fe',
				backgroundColor: 'rgba(15, 98, 254, 0.08)',
				fill: true,
				tension: 0.1,
				pointRadius: 0,
				borderWidth: 1.5,
				spanGaps: false
			}
		]
	});

	let lastValidPoint = $derived(
		[...series.points].reverse().find((p) => p.quality === 'valid')
	);

	let unitSuffix = $derived.by(() => {
		if (series.unit === 'bytes') return series.kind === 'counter' ? 'B/s' : 'B';
		if (series.unit === 'bytes_per_second') return 'B/s';
		if (series.unit === 'ratio') return '%';
		return series.unit;
	});

	let coveragePercent = $derived(Math.round(series.coverage_ratio * 100));

	const hasPoints = $derived(series.points.length > 0);
</script>

<div class="metric-chart">
	<div class="chart-header">
		<div class="chart-title">
			<span class="title-text">{title}</span>
			<span class="unit-badge">{unitSuffix}</span>
		</div>
		<div class="chart-meta">
			{#if series.source}
				<span class="meta-item">source: {sourceLabel(series.source)}</span>
			{/if}
			{#if lastValidPoint}
				<span class="meta-item">last point: {formatAge(lastValidPoint.timestamp_ms, nowMs)}</span>
			{/if}
			{#if hasPoints}
				<span class="meta-item">coverage: {coveragePercent}%</span>
			{/if}
			{#if series.truncated}
				<span class="meta-item">downsampled to {series.points.length} points</span>
			{/if}
		</div>
	</div>

	{#if hasPoints}
		<div class="chart-body">
			<ChartJS
				type="line"
				data={chartData}
				height={180}
				options={{
					plugins: {
						legend: { display: false },
						tooltip: {
							callbacks: {
								label: (item: { dataIndex: number }) => {
									const point = series.points[item.dataIndex];
									if (!point || point.quality !== 'valid') {
										return point ? `no data (${qualityLabel(point.quality)})` : '';
									}
									if (series.kind === 'counter') {
										const rate = formatCounterRate(point.integer_value, point.window_ms);
										return rate !== null
											? `${rate.toFixed(1)} ${unitSuffix} (average rate)`
											: 'no data';
									}
									return `${point.value ?? '—'} ${unitSuffix}`;
								}
							}
						}
					},
					scales: {
						x: {
							grid: { display: false },
							ticks: { font: { size: 9, family: 'var(--chv-font-mono)' }, maxTicksLimit: 8 }
						},
						y: {
							grid: { color: 'rgba(0,0,0,0.05)' },
							ticks: { font: { size: 9, family: 'var(--chv-font-mono)' } },
							beginAtZero: series.kind !== 'gauge' || series.unit === 'bytes'
						}
					}
				}}
			/>
		</div>
	{:else}
		<div class="chart-empty">
			<span class="empty-title">{reasonLabel(series.reason)}</span>
			<span class="empty-hint">
				{#if series.source}
					{sourceLabel(series.source)} reports no stored samples for this metric.
				{:else}
					No source has collected this metric for this target.
				{/if}
			</span>
		</div>
	{/if}
</div>

<style>
	.metric-chart {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		padding: 1rem;
		background: var(--bg-surface);
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-xs);
	}

	.chart-header {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
	}

	.chart-title {
		display: flex;
		align-items: center;
		gap: 0.5rem;
	}

	.title-text {
		font-size: 12px;
		font-weight: 700;
		color: var(--chv-color-neutral-900);
	}

	.unit-badge {
		font-size: 9px;
		font-family: var(--chv-font-mono);
		font-weight: 700;
		padding: 1px 5px;
		border-radius: 2px;
		background: var(--bg-surface-muted);
		color: var(--chv-color-neutral-600);
	}

	.chart-meta {
		display: flex;
		flex-wrap: wrap;
		gap: 0.75rem;
	}

	.meta-item {
		font-size: 9px;
		font-family: var(--chv-font-mono);
		color: var(--chv-color-neutral-500);
	}

	.chart-body {
		position: relative;
		height: 180px;
	}

	.chart-empty {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
		align-items: center;
		justify-content: center;
		height: 180px;
		background: var(--bg-surface-muted);
		border-radius: var(--radius-xs);
	}

	.empty-title {
		font-size: 11px;
		font-weight: 700;
		color: var(--chv-color-neutral-500);
	}

	.empty-hint {
		font-size: 10px;
		color: var(--chv-color-neutral-400);
	}
</style>
