<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { BarChart3 } from 'lucide-svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import TimeRangePicker from '#lib/components/monitoring/TimeRangePicker.svelte';
	import MetricChart from '#lib/components/monitoring/MetricChart.svelte';
	import {
		fetchMonitoringHistory,
		type MonitoringSeries,
		type MonitoringTimeRange
	} from '#lib/bff/monitoring.ts';
	import { BFFError } from '#lib/bff/client.ts';

	interface Props {
		targetKind: 'node' | 'vm';
		targetId: string;
		metricIds: string[];
	}

	let { targetKind, targetId, metricIds }: Props = $props();

	let selectedRange = $state<MonitoringTimeRange>('1h');
	let series = $state<MonitoringSeries[]>([]);
	let loading = $state(true);
	/** 'degraded' = the monitoring subsystem is unavailable (503); it
	 * never means the target is unhealthy. */
	let degraded = $state(false);
	let error = $state<string | null>(null);
	let nowMs = $state(Date.now());
	let pollInterval: number | null = null;

	async function load() {
		loading = series.length === 0;
		try {
			const response = await fetchMonitoringHistory(
				targetKind,
				targetId,
				metricIds,
				selectedRange
			);
			// A response that is not the documented shape (a proxy or
			// misrouted gateway answering with its own JSON) is a
			// request error — never a crash, and never a silent
			// "no data" that would misread an outage as an absence.
			if (!response || !Array.isArray(response.series)) {
				degraded = false;
				error = 'Monitoring service returned an unexpected response';
				series = [];
				return;
			}
			series = response.series;
			nowMs = response.generated_at_ms;
			degraded = false;
			error = null;
		} catch (err) {
			if (err instanceof BFFError && err.status === 503) {
				degraded = true;
				error = null;
			} else {
				degraded = false;
				error = err instanceof Error ? err.message : 'Monitoring request failed';
			}
		} finally {
			loading = false;
		}
	}

	function setRange(range: MonitoringTimeRange) {
		if (range === selectedRange) return;
		selectedRange = range;
		series = [];
		load();
	}

	onMount(() => {
		load();
		pollInterval = window.setInterval(load, 30_000);
	});

	onDestroy(() => {
		if (pollInterval) clearInterval(pollInterval);
	});
</script>

<SectionCard title="Monitoring" icon={BarChart3}>
	<div class="panel-header">
		<span class="source-note">
			{targetKind === 'vm'
				? 'Host-accounted measurements from the VMM process; guest-visible metrics appear only when a guest agent reports them'
				: 'Node OS measurements, collected by the agent on this node'}
		</span>
		<TimeRangePicker value={selectedRange} onChange={setRange} />
	</div>

	{#if loading}
		<div class="state-box">
			<span class="state-title">Loading monitoring data…</span>
		</div>
	{:else if degraded}
		<div class="state-box state-degraded">
			<span class="state-title">Monitoring is unavailable</span>
			<span class="state-hint">
				The monitoring subsystem is degraded or disabled on the control plane.
				This does not indicate a problem with this {targetKind === 'vm' ? 'VM' : 'node'}.
			</span>
		</div>
	{:else if error}
		<div class="state-box state-error">
			<span class="state-title">Monitoring request failed</span>
			<span class="state-hint">{error}</span>
		</div>
	{:else if series.length === 0}
		<div class="state-box">
			<span class="state-title">No monitoring data</span>
			<span class="state-hint">
				No metric has been collected for this {targetKind === 'vm' ? 'VM' : 'node'} yet.
			</span>
		</div>
	{:else}
		<div class="charts-grid">
			{#each series as s (s.metric_id + '|' + s.source + '|' + JSON.stringify(s.dimensions))}
				<MetricChart series={s} {nowMs} />
			{/each}
		</div>
	{/if}
</SectionCard>

<style>
	.panel-header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 0.75rem;
		flex-wrap: wrap;
		margin-bottom: 0.75rem;
	}

	.source-note {
		font-size: 10px;
		color: var(--chv-color-neutral-500);
	}

	.charts-grid {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(320px, 1fr));
		gap: 0.75rem;
	}

	.state-box {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
		align-items: center;
		justify-content: center;
		padding: 2.5rem 1rem;
		background: var(--bg-surface-muted);
		border-radius: var(--radius-xs);
	}

	.state-degraded {
		border: 1px dashed var(--chv-color-neutral-300, #d4d4d4);
	}

	.state-error {
		border: 1px solid var(--color-danger, #dc2626);
	}

	.state-title {
		font-size: 12px;
		font-weight: 700;
		color: var(--chv-color-neutral-600);
	}

	.state-error .state-title {
		color: var(--color-danger, #dc2626);
	}

	.state-hint {
		font-size: 10px;
		color: var(--chv-color-neutral-400);
		text-align: center;
		max-width: 40rem;
	}
</style>
