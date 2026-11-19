<script lang="ts">
	import { onDestroy, onMount } from 'svelte';
	import { ListTree } from 'lucide-svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import Badge from '#lib/components/primitives/Badge.svelte';
	import { fetchMonitoringCurrent } from '#lib/bff/monitoring.ts';
	import { BFFError } from '#lib/bff/client.ts';
	import { formatBytes, formatRatio } from '#lib/monitoring/format.ts';
	import {
		GUEST_PROCESS_METRIC_IDS,
		guestProcessRows,
		type GuestProcessRow
	} from '#lib/monitoring/guestChecks.ts';

	interface Props {
		vmId: string;
	}

	let { vmId }: Props = $props();

	let rows = $state<GuestProcessRow[]>([]);
	let loading = $state(true);
	/** 'degraded' = the monitoring subsystem is unavailable (503); it
	 * never means the VM is unhealthy. */
	let degraded = $state(false);
	let error = $state<string | null>(null);
	let pollInterval: number | null = null;

	async function load() {
		loading = rows.length === 0;
		try {
			const response = await fetchMonitoringCurrent(
				'vm',
				vmId,
				[...GUEST_PROCESS_METRIC_IDS]
			);
			// A response that is not the documented shape is a request
			// error — never a silent "no data" that would misread an
			// outage as an absence.
			if (!response || !Array.isArray(response.samples)) {
				degraded = false;
				error = 'Monitoring service returned an unexpected response';
				rows = [];
				return;
			}
			rows = guestProcessRows(response.samples);
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

	onMount(() => {
		load();
		pollInterval = window.setInterval(load, 30_000);
	});

	onDestroy(() => {
		if (pollInterval) clearInterval(pollInterval);
	});
</script>

<SectionCard title="Guest processes">
	{#snippet icon()}
		<ListTree class="h-4 w-4" aria-hidden="true" />
	{/snippet}

	<p class="text-[10px] text-(--shell-text-muted) mb-2">
		Reported by the optional guest monitoring agent — guest-observed, never host-accounted.
	</p>

	{#if loading}
		<p class="text-sm text-(--shell-text-muted)">Loading process telemetry…</p>
	{:else if degraded}
		<div class="flex flex-col items-center gap-1 py-8 px-4 rounded-sm bg-(--shell-surface-muted) border border-dashed border-(--shell-line)">
			<span class="text-xs font-bold text-(--shell-text-secondary)">Monitoring is unavailable</span>
			<span class="text-[10px] text-(--shell-text-muted) text-center max-w-160">
				The monitoring subsystem is degraded or disabled on the control plane. This does not
				indicate a problem with this VM.
			</span>
		</div>
	{:else if error}
		<div class="flex flex-col items-center gap-1 py-8 px-4 rounded-sm bg-(--shell-surface-muted) border border-danger">
			<span class="text-xs font-bold text-danger">Monitoring request failed</span>
			<span class="text-[10px] text-(--shell-text-muted) text-center max-w-160">{error}</span>
		</div>
	{:else if rows.length === 0}
		<p class="text-sm text-(--shell-text-secondary)">
			No process telemetry for this guest. Process selectors are an opt-in guest agent
			configuration — processes are not collected by default, for guest privacy.
		</p>
	{:else}
		<div class="overflow-x-auto">
			<table class="w-full text-xs border-collapse">
				<thead>
					<tr class="text-left text-(--shell-text-muted) border-b border-(--shell-line)">
						<th class="py-1.5 pr-3 font-semibold">Selector</th>
						<th class="py-1.5 pr-3 font-semibold">Processes</th>
						<th class="py-1.5 pr-3 font-semibold">CPU</th>
						<th class="py-1.5 font-semibold">RSS</th>
					</tr>
				</thead>
				<tbody>
					{#each rows as row (row.selector)}
						<tr class="border-b border-(--shell-line) last:border-b-0">
							<td class="py-1.5 pr-3 font-medium text-(--shell-text) whitespace-nowrap">
								{row.selector}
								{#if row.stale}
									<Badge>stale</Badge>
								{/if}
							</td>
							<td class="py-1.5 pr-3 text-(--shell-text-secondary) tabular-nums">
								{row.count !== null ? row.count : '—'}
							</td>
							<td class="py-1.5 pr-3 text-(--shell-text-secondary) tabular-nums">
								{row.cpuRatio !== null ? formatRatio(row.cpuRatio) : '—'}
							</td>
							<td class="py-1.5 text-(--shell-text-secondary) tabular-nums">
								{row.rssBytes !== null ? formatBytes(row.rssBytes) : '—'}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</SectionCard>
