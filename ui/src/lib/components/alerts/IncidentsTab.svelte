<script lang="ts">
	import { Check, BellOff, ChevronRight } from 'lucide-svelte';
	import Badge from '#lib/components/primitives/Badge.svelte';
	import Button from '#lib/components/primitives/Button.svelte';
	import ErrorState from '#lib/components/shell/ErrorState.svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import {
		acknowledgeIncident,
		silenceIncident,
		type Incident
	} from '#lib/bff/alerting.ts';
	import { BFFError } from '#lib/bff/client.ts';
	import { getStoredToken } from '#lib/api/client.ts';
	import { mutateWithRefresh } from '#lib/stores/mutation.svelte.ts';
	import {
		SILENCE_PICKS,
		formatAlertTimestamp,
		incidentStateView,
		severityBadgeVariant,
		silenceRemainingLabel,
		targetHref
	} from '#lib/alerts/rules.ts';
	import IncidentDetailDrawer from './IncidentDetailDrawer.svelte';

	interface Props {
		incidents: Incident[];
		total: number;
		loading: boolean;
		error: boolean;
		isOperator: boolean;
		/** Bindable: the current "show resolved" state; flipping it reloads. */
		includeResolved?: boolean;
		onReload: (includeResolved: boolean) => Promise<void>;
	}

	let {
		incidents,
		total,
		loading,
		error,
		isOperator,
		includeResolved = $bindable(false),
		onReload
	}: Props = $props();

	let selectedAlertId = $state<string | null>(null);
	let busyAlertId = $state<string | null>(null);

	async function toggleResolved() {
		includeResolved = !includeResolved;
		await onReload(includeResolved);
	}

	async function acknowledge(incident: Incident) {
		const alertId = incident.alert_id;
		busyAlertId = alertId;
		try {
			await mutateWithRefresh(
				() => acknowledgeIncident({ alert_id: alertId }, getStoredToken() ?? undefined),
				{
					skipRefresh: true,
					successMessage: 'Incident acknowledged',
					errorMessage: 'Failed to acknowledge incident'
				}
			);
			await onReload(includeResolved);
			// Ack state is part of the drawer's incident view too.
			if (selectedAlertId === alertId) selectedAlertId = null;
		} catch {
			// Error already toasted by mutateWithRefresh.
		} finally {
			busyAlertId = null;
		}
	}

	async function silence(incident: Incident, minutes: number) {
		const alertId = incident.alert_id;
		busyAlertId = alertId;
		try {
			await mutateWithRefresh(
				() =>
					silenceIncident(
						{ alert_id: alertId, duration_minutes: minutes },
						getStoredToken() ?? undefined
					),
				{
					skipRefresh: true,
					successMessage: `Incident silenced for ${minutes} minutes`,
					errorMessage: 'Failed to silence incident'
				}
			);
			await onReload(includeResolved);
			if (selectedAlertId === alertId) selectedAlertId = null;
		} catch (err) {
			// A 409/404 here means the incident already resolved and the
			// row is stale — reload so the list tells the truth.
			if (err instanceof BFFError && (err.status === 404 || err.status === 409)) {
				await onReload(includeResolved);
			}
		} finally {
			busyAlertId = null;
		}
	}
</script>

<SectionCard title="Incidents" badgeLabel={String(total)}>
	{#snippet actions()}
		<label class="flex items-center gap-2 text-[10px] font-bold uppercase tracking-wider text-(--shell-text-muted) cursor-pointer m-0">
			<input type="checkbox" checked={includeResolved} onchange={toggleResolved} />
			Show resolved
		</label>
	{/snippet}

	{#if error}
		<ErrorState />
	{:else if loading && incidents.length === 0}
		<p class="text-sm text-(--shell-text-muted)">Loading incidents…</p>
	{:else if incidents.length === 0}
		<p class="text-sm text-(--shell-text-secondary)">
			No {includeResolved ? '' : 'active '}incidents. Rules evaluate every few seconds;
			pending incidents appear here before any notification is sent.
		</p>
	{:else}
		<div class="overflow-x-auto">
			<table class="w-full text-xs border-collapse">
				<thead>
					<tr class="text-left text-(--shell-text-muted) border-b border-(--shell-line)">
						<th class="py-1.5 pr-3 font-semibold">State</th>
						<th class="py-1.5 pr-3 font-semibold">Severity</th>
						<th class="py-1.5 pr-3 font-semibold">Target</th>
						<th class="py-1.5 pr-3 font-semibold">Message</th>
						<th class="py-1.5 pr-3 font-semibold">Last observed</th>
						<th class="py-1.5 pr-3 font-semibold">Opened</th>
						{#if isOperator}
							<th class="py-1.5 font-semibold text-right">Actions</th>
						{/if}
					</tr>
				</thead>
				<tbody>
					{#each incidents as incident (incident.alert_id)}
						{@const state = incidentStateView(incident.status)}
						{@const silenced =
							incident.silenced_until_ms !== null && incident.silenced_until_ms > Date.now()}
						<tr
							class="border-b border-(--shell-line) last:border-b-0 cursor-pointer hover:bg-(--shell-surface-muted)"
							onclick={() => (selectedAlertId = incident.alert_id)}
						>
							<td class="py-1.5 pr-3 whitespace-nowrap">
								<span class="inline-flex items-center gap-1.5">
									<Badge variant={state.tone} dot>{state.label}</Badge>
									{#if incident.acknowledged_at}
										<Badge variant="info">ack</Badge>
									{/if}
									{#if silenced}
										<Badge>silenced {silenceRemainingLabel(incident.silenced_until_ms, Date.now())}</Badge>
									{/if}
								</span>
							</td>
							<td class="py-1.5 pr-3 whitespace-nowrap">
								<Badge variant={severityBadgeVariant(incident.severity)}>{incident.severity}</Badge>
							</td>
							<td class="py-1.5 pr-3 whitespace-nowrap">
								<a
									href={targetHref(incident.target_kind, incident.target_id)}
									class="text-(--shell-accent) no-underline hover:underline"
									onclick={(event) => event.stopPropagation()}
								>
									{incident.target_kind}/{incident.target_id}
								</a>
							</td>
							<!-- The message is the evaluator's rendered summary; plain text only. -->
							<td class="py-1.5 pr-3 text-(--shell-text-secondary) max-w-88 truncate">
								{incident.message}
							</td>
							<td class="py-1.5 pr-3 text-(--shell-text-secondary) whitespace-nowrap">
								{incident.last_observed ?? '—'}
							</td>
							<td class="py-1.5 pr-3 text-(--shell-text-secondary) whitespace-nowrap">
								{formatAlertTimestamp(incident.opened_at)}
							</td>
							{#if isOperator && incident.status !== 'resolved'}
								<td class="py-1.5 text-right whitespace-nowrap">
									<Button
										variant="ghost"
										size="sm"
										ariaLabel="Acknowledge incident"
										title="Acknowledge"
										disabled={busyAlertId === incident.alert_id || !!incident.acknowledged_at}
										onclick={(event) => {
											event.stopPropagation();
											acknowledge(incident);
										}}
									>
										<Check size={13} />
									</Button>
									<details class="relative">
										<summary
											class="list-none cursor-pointer inline-flex items-center gap-1 px-2 h-8 rounded-xs text-neutral-600 hover:bg-neutral-100 hover:text-neutral-900 text-sm font-medium"
											title="Silence"
											onclick={(event) => event.stopPropagation()}
										>
											<BellOff size={13} />
											<ChevronRight size={11} />
										</summary>
										<div class="absolute right-0 z-10 mt-1 flex flex-col gap-1 p-1 bg-(--bg-surface) border border-neutral-300 rounded-xs shadow-lg min-w-20">
											{#each SILENCE_PICKS as pick (pick.minutes)}
												<button
													type="button"
													class="text-xs text-left px-2 py-1 rounded-xs bg-transparent border-none cursor-pointer text-neutral-700 hover:bg-neutral-100"
													disabled={busyAlertId === incident.alert_id}
													onclick={(event) => {
														event.stopPropagation();
														silence(incident, pick.minutes);
													}}
												>
													Silence {pick.label}
												</button>
											{/each}
										</div>
									</details>
								</td>
							{:else if isOperator}
								<td class="py-1.5 text-right text-(--shell-text-muted)">—</td>
							{/if}
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</SectionCard>

<IncidentDetailDrawer
	alertId={selectedAlertId}
	{isOperator}
	onClose={() => (selectedAlertId = null)}
	onAcknowledge={acknowledge}
	onSilence={silence}
/>
