<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import { Activity } from 'lucide-svelte';
	import { getMonitoringHealth, type MonitoringHealthResponse } from '#lib/bff/monitoring.ts';
	import { BFFError } from '#lib/bff/client.ts';
	import { formatAge, formatBytes } from '#lib/monitoring/format.ts';

	let health = $state<MonitoringHealthResponse | null>(null);
	let failed = $state(false);
	let nowMs = $state(Date.now());
	let pollInterval: number | null = null;

	async function load() {
		try {
			const response = await getMonitoringHealth();
			// A response that is not the documented shape (a proxy or
			// misrouted gateway) is a failure — never a guessed state.
			if (!response || typeof response.available !== 'boolean') {
				throw new Error('unexpected response shape');
			}
			health = response;
			failed = false;
			nowMs = Date.now();
		} catch {
			// A failed request is a failed request — the card says so,
			// it never guesses a state.
			failed = true;
		}
	}

	onMount(() => {
		load();
		pollInterval = window.setInterval(load, 30_000);
	});

	onDestroy(() => {
		if (pollInterval) clearInterval(pollInterval);
	});

	let statusLabel = $derived.by(() => {
		if (failed) return 'unreachable';
		if (!health) return 'loading';
		if (!health.available) return 'degraded';
		if (health.degraded_reason) return 'degraded';
		return 'ok';
	});

	let statusTone = $derived(
		statusLabel === 'ok' ? 'ok' : statusLabel === 'loading' ? 'muted' : 'warn'
	);
</script>

<SectionCard title="Monitoring Health" icon={Activity}>
	<div class="monitoring-health">
		<div class="status-row">
			<span class="status-dot {statusTone}"></span>
			<span class="status-label">{statusLabel === 'ok' ? 'Operational' : statusLabel === 'degraded' ? 'Degraded' : statusLabel === 'unreachable' ? 'Unreachable' : 'Loading…'}</span>
		</div>

		{#if health}
			{#if health.available}
				<div class="health-row">
					<span>Last ingest</span>
					<strong>{health.last_ingest_at_ms ? formatAge(health.last_ingest_at_ms, nowMs) : 'never'}</strong>
				</div>
				{#if health.raw_samples !== undefined}
					<div class="health-row">
						<span>Stored samples</span>
						<strong>{new Intl.NumberFormat().format(health.raw_samples)}</strong>
					</div>
				{/if}
				{#if health.accepted_batches !== undefined}
					<div class="health-row">
						<span>Batches accepted / duplicate</span>
						<strong>{health.accepted_batches} / {health.duplicate_batches ?? 0}</strong>
					</div>
				{/if}
				{#if (health.rejected_batches ?? 0) > 0 || (health.unavailable_batches ?? 0) > 0}
					<div class="health-row warn">
						<span>Rejected / dropped</span>
						<strong>{health.rejected_batches ?? 0} / {health.unavailable_batches ?? 0}</strong>
					</div>
				{/if}
				{#if health.headroom_bytes !== null && health.headroom_bytes !== undefined}
					<div class="health-row">
						<span>Store headroom</span>
						<strong>{formatBytes(health.headroom_bytes)}</strong>
					</div>
				{/if}
				{#if health.headroom_probe_failed}
					<div class="health-row warn">
						<span>Headroom probe</span>
						<strong>failed — floor unverified</strong>
					</div>
				{/if}
			{:else}
				<div class="degraded-note">
					The monitoring history subsystem is unavailable{health.degraded_reason ? `: ${health.degraded_reason}` : '.'}
					This does not indicate a problem with nodes or VMs — lifecycle and
					reconciliation are unaffected.
				</div>
			{/if}
		{:else if failed}
			<div class="degraded-note">
				The monitoring health endpoint did not answer. This does not indicate a
				problem with nodes or VMs.
			</div>
		{/if}
	</div>
</SectionCard>

<style>
	.monitoring-health {
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
	}

	.status-row {
		display: flex;
		align-items: center;
		gap: 0.4rem;
	}

	.status-dot {
		width: 7px;
		height: 7px;
		border-radius: 50%;
	}

	.status-dot.ok {
		background: var(--color-success, #059669);
	}

	.status-dot.warn {
		background: var(--color-warning, #d97706);
	}

	.status-dot.muted {
		background: var(--chv-color-neutral-400, #a3a3a3);
	}

	.status-label {
		font-size: 11px;
		font-weight: 700;
		color: var(--chv-color-neutral-700);
	}

	.health-row {
		display: flex;
		justify-content: space-between;
		font-size: 10px;
		color: var(--chv-color-neutral-600);
		padding: 0.3rem 0.4rem;
		background: var(--bg-surface-muted);
		border-radius: var(--radius-xs);
	}

	.health-row strong {
		font-weight: 700;
		color: var(--chv-color-neutral-900);
	}

	.health-row.warn strong {
		color: var(--color-warning, #d97706);
	}

	.degraded-note {
		font-size: 10px;
		line-height: 1.5;
		color: var(--chv-color-neutral-500);
		padding: 0.4rem;
		background: var(--bg-surface-muted);
		border-radius: var(--radius-xs);
	}
</style>
