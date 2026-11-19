<script lang="ts">
	import { onDestroy, onMount } from 'svelte';
	import { HardDrive } from 'lucide-svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import Badge from '#lib/components/primitives/Badge.svelte';
	import { fetchMonitoringCurrent } from '#lib/bff/monitoring.ts';
	import { BFFError } from '#lib/bff/client.ts';
	import { formatBytes, formatRatio } from '#lib/monitoring/format.ts';
	import {
		GUEST_FS_METRIC_IDS,
		guestFsRows,
		type GuestFsRow
	} from '#lib/monitoring/guestChecks.ts';

	interface Props {
		vmId: string;
	}

	let { vmId }: Props = $props();

	let rows = $state<GuestFsRow[]>([]);
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
				[...GUEST_FS_METRIC_IDS]
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
			rows = guestFsRows(response.samples);
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

<SectionCard title="Guest filesystems">
	{#snippet icon()}
		<HardDrive class="h-4 w-4" aria-hidden="true" />
	{/snippet}

	<p class="text-[10px] text-(--shell-text-muted) mb-2">
		Reported by the optional guest monitoring agent — guest-observed, never host-accounted.
	</p>

	{#if loading}
		<p class="text-sm text-(--shell-text-muted)">Loading filesystem telemetry…</p>
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
			No filesystem telemetry for this guest. Filesystem data appears only when the optional
			guest monitoring agent is installed and its filesystem collector is enabled.
		</p>
	{:else}
		<div class="overflow-x-auto">
			<table class="w-full text-xs border-collapse">
				<thead>
					<tr class="text-left text-(--shell-text-muted) border-b border-(--shell-line)">
						<th class="py-1.5 pr-3 font-semibold">Mount</th>
						<th class="py-1.5 pr-3 font-semibold">Type</th>
						<th class="py-1.5 pr-3 font-semibold">Total</th>
						<th class="py-1.5 pr-3 font-semibold">Used</th>
						<th class="py-1.5 pr-3 font-semibold">Available</th>
						<th class="py-1.5 pr-3 font-semibold">Usage</th>
						<th class="py-1.5 font-semibold">Inodes</th>
					</tr>
				</thead>
				<tbody>
					{#each rows as row (row.mountId)}
						<tr class="border-b border-(--shell-line) last:border-b-0">
							<td class="py-1.5 pr-3 font-medium text-(--shell-text) whitespace-nowrap">
								{row.mountpoint}
								{#if row.readOnly}
									<Badge>read-only</Badge>
								{/if}
								{#if row.stale}
									<Badge>stale</Badge>
								{/if}
							</td>
							<td class="py-1.5 pr-3 text-(--shell-text-secondary)">{row.fstype || '—'}</td>
							<td class="py-1.5 pr-3 text-(--shell-text-secondary) tabular-nums">
								{row.totalBytes !== null ? formatBytes(row.totalBytes) : '—'}
							</td>
							<td class="py-1.5 pr-3 text-(--shell-text-secondary) tabular-nums">
								{row.usedBytes !== null ? formatBytes(row.usedBytes) : '—'}
							</td>
							<td class="py-1.5 pr-3 text-(--shell-text-secondary) tabular-nums">
								{row.availableBytes !== null ? formatBytes(row.availableBytes) : '—'}
							</td>
							<td class="py-1.5 pr-3 w-40">
								{#if row.usageRatio !== null}
									<div class="flex items-center gap-2">
										<div class="h-1.5 flex-1 rounded-full bg-(--shell-line) overflow-hidden">
											<div
												class="h-full rounded-full bg-(--shell-text-secondary)"
												style:width="{Math.max(0, Math.min(100, Math.round(row.usageRatio * 100)))}%"
											></div>
										</div>
										<span class="tabular-nums text-(--shell-text-secondary) whitespace-nowrap">
											{formatRatio(row.usageRatio)}
										</span>
									</div>
								{:else}
									<span class="text-(--shell-text-muted)">—</span>
								{/if}
							</td>
							<td class="py-1.5 text-(--shell-text-secondary) tabular-nums">
								{row.inodeRatio !== null ? formatRatio(row.inodeRatio) : '—'}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</SectionCard>
