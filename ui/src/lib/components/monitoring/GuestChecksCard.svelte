<script lang="ts">
	import { onDestroy, onMount } from 'svelte';
	import { ClipboardCheck } from 'lucide-svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import Badge from '#lib/components/primitives/Badge.svelte';
	import {
		fetchMonitoringChecks,
		type MonitoringCheck
	} from '#lib/bff/monitoring.ts';
	import { BFFError } from '#lib/bff/client.ts';
	import {
		checkAgeLabel,
		checkStatusView,
		parseCheckId,
		sortChecks
	} from '#lib/monitoring/guestChecks.ts';

	interface Props {
		vmId: string;
	}

	let { vmId }: Props = $props();

	let checks = $state<MonitoringCheck[]>([]);
	let loading = $state(true);
	/** 'degraded' = the monitoring subsystem is unavailable (503); it
	 * never means the guest is unhealthy. */
	let degraded = $state(false);
	let error = $state<string | null>(null);
	let nowMs = $state(Date.now());
	let pollInterval: number | null = null;

	/** The check-status tone mapped onto the Badge variant vocabulary. */
	const badgeVariant: Record<string, 'default' | 'success' | 'warning' | 'danger'> = {
		success: 'success',
		warning: 'warning',
		danger: 'danger',
		muted: 'default'
	};

	async function load() {
		loading = checks.length === 0;
		try {
			const response = await fetchMonitoringChecks('vm', vmId);
			// A response that is not the documented shape is a request
			// error — never a silent "no checks" that would misread an
			// outage as "everything fine".
			if (!response || !Array.isArray(response.checks)) {
				degraded = false;
				error = 'Monitoring service returned an unexpected response';
				checks = [];
				return;
			}
			checks = sortChecks(response.checks);
			nowMs = response.generated_at_ms || Date.now();
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

<SectionCard title="Guest checks">
	{#snippet icon()}
		<ClipboardCheck class="h-4 w-4" aria-hidden="true" />
	{/snippet}

	<p class="text-[10px] text-(--shell-text-muted) mb-2">
		Reported by the optional guest monitoring agent — guest-observed, never host-accounted.
	</p>

	{#if loading}
		<p class="text-sm text-(--shell-text-muted)">Loading guest checks…</p>
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
	{:else if checks.length === 0}
		<p class="text-sm text-(--shell-text-secondary)">No checks configured for this guest.</p>
	{:else}
		<div class="overflow-x-auto">
			<table class="w-full text-xs border-collapse">
				<thead>
					<tr class="text-left text-(--shell-text-muted) border-b border-(--shell-line)">
						<th class="py-1.5 pr-3 font-semibold">Check</th>
						<th class="py-1.5 pr-3 font-semibold">Status</th>
						<th class="py-1.5 pr-3 font-semibold">Summary</th>
						<th class="py-1.5 font-semibold">Observed</th>
					</tr>
				</thead>
				<tbody>
					{#each checks as check (check.check_id)}
						{@const parts = parseCheckId(check.check_id)}
						{@const view = checkStatusView(check.status)}
						<tr class="border-b border-(--shell-line) last:border-b-0">
							<td class="py-1.5 pr-3 text-(--shell-text) whitespace-nowrap">
								<span class="text-(--shell-text-muted)">{parts.kindLabel}</span>
								<span class="mx-1" aria-hidden="true">·</span>
								<span class="font-medium">{parts.name}</span>
							</td>
							<td class="py-1.5 pr-3 whitespace-nowrap">
								<Badge variant={badgeVariant[view.tone]} dot>{view.label}</Badge>
								{#if check.stale}
									<Badge>stale</Badge>
								{/if}
							</td>
							<!-- Summaries are untrusted guest-side strings: plain text
							     interpolation only, never {@html}. -->
							<td class="py-1.5 pr-3 text-(--shell-text-secondary)">
								{check.summary ?? '—'}
							</td>
							<td class="py-1.5 text-(--shell-text-secondary) whitespace-nowrap">
								{checkAgeLabel(check.observed_at_ms, nowMs)}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</SectionCard>
