<script lang="ts">
	import { untrack } from 'svelte';
	import { X, Check, BellOff } from 'lucide-svelte';
	import Button from '#lib/components/primitives/Button.svelte';
	import Badge from '#lib/components/primitives/Badge.svelte';
	import {
		incidentDetail,
		type Incident,
		type IncidentTransition
	} from '#lib/bff/alerting.ts';
	import { getStoredToken } from '#lib/api/client.ts';
	import {
		SILENCE_PICKS,
		formatAlertTimestamp,
		incidentStateView,
		severityBadgeVariant,
		silenceRemainingLabel,
		targetHref
	} from '#lib/alerts/rules.ts';

	interface Props {
		alertId: string | null;
		isOperator: boolean;
		onClose: () => void;
		onAcknowledge: (incident: Incident) => void | Promise<void>;
		onSilence: (incident: Incident, minutes: number) => void | Promise<void>;
	}

	let { alertId, isOperator, onClose, onAcknowledge, onSilence }: Props = $props();

	let incident = $state<Incident | null>(null);
	let transitions = $state<IncidentTransition[]>([]);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let busy = $state(false);

	async function load() {
		if (!alertId) return;
		loading = incident === null;
		error = null;
		try {
			const response = await incidentDetail(
				{ alert_id: alertId },
				getStoredToken() ?? undefined
			);
			incident = response.incident;
			transitions = response.transitions ?? [];
		} catch (err) {
			error = err instanceof Error ? err.message : 'Incident detail unavailable';
			incident = null;
			transitions = [];
		} finally {
			loading = false;
		}
	}

	// Reload whenever the drawer is pointed at another incident. The
	// reset + fetch must not be tracked: `load` reads `incident` for
	// its loading flag, and tracking it here would re-run this effect
	// after every fetch and loop forever.
	$effect(() => {
		const id = alertId;
		if (id) {
			untrack(() => {
				incident = null;
				transitions = [];
				load();
			});
		}
	});

	function onKeydown(event: KeyboardEvent) {
		if (event.key === 'Escape') onClose();
	}
</script>

<svelte:window onkeydown={onKeydown} />

{#if alertId}
	<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
	<div
		class="fixed inset-0 z-40 bg-black/40 flex justify-end"
		role="presentation"
		onclick={(event) => {
			if (event.target === event.currentTarget) onClose();
		}}
	>
		<div
			class="h-full w-full max-w-136 bg-(--shell-surface) border-l border-(--shell-line) overflow-y-auto"
			role="dialog"
			aria-label="Incident detail"
			tabindex="-1"
		>
			<header class="flex items-start justify-between gap-3 px-4 py-3 border-b border-(--shell-line) bg-(--shell-surface-muted) sticky top-0">
				<div class="min-w-0">
					<h3 class="text-(length:--chv-text-sm) font-bold text-(--shell-text) m-0">
						Incident detail
					</h3>
					<p class="text-[10px] text-(--shell-text-muted) m-0 mt-0.5 break-all">
						{alertId}
					</p>
				</div>
				<Button variant="ghost" size="sm" onclick={onClose} ariaLabel="Close incident detail">
					<X size={14} />
				</Button>
			</header>

			<div class="p-4 flex flex-col gap-4">
				{#if loading}
					<p class="text-sm text-(--shell-text-muted)">Loading incident…</p>
				{:else if error}
					<p class="text-sm text-danger">{error}</p>
				{:else if incident}
					{@const state = incidentStateView(incident.status)}
					{@const silenced =
						incident.silenced_until_ms !== null && incident.silenced_until_ms > Date.now()}
					<div class="flex items-center gap-2 flex-wrap">
						<Badge variant={state.tone} dot>
							{state.label}
						</Badge>
						<Badge variant={severityBadgeVariant(incident.severity)}>{incident.severity}</Badge>
						{#if incident.acknowledged_at}
							<Badge variant="info">ack {incident.acknowledged_by ?? ''}</Badge>
						{/if}
						{#if silenced}
							<Badge variant="default">silenced · {silenceRemainingLabel(incident.silenced_until_ms, Date.now())}</Badge>
						{/if}
					</div>

					<!-- The message is the evaluator's rendered summary; plain text only. -->
					<p class="text-sm text-(--shell-text) m-0">{incident.message}</p>

					<dl class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-xs">
						<dt class="text-(--shell-text-muted)">Target</dt>
						<dd class="text-(--shell-text-secondary)">
							<a
								href={targetHref(incident.target_kind, incident.target_id)}
								class="text-(--shell-accent) no-underline hover:underline"
							>
								{incident.target_kind}/{incident.target_id} →
							</a>
						</dd>
						<dt class="text-(--shell-text-muted)">Last observed</dt>
						<dd class="text-(--shell-text-secondary)">{incident.last_observed ?? '—'}</dd>
						<dt class="text-(--shell-text-muted)">Opened</dt>
						<dd class="text-(--shell-text-secondary)">{formatAlertTimestamp(incident.opened_at)}</dd>
						{#if incident.resolved_at}
							<dt class="text-(--shell-text-muted)">Resolved</dt>
							<dd class="text-(--shell-text-secondary)">{formatAlertTimestamp(incident.resolved_at)}</dd>
						{/if}
						<dt class="text-(--shell-text-muted)">Evidence window</dt>
						<dd class="text-(--shell-text-secondary)">
							{formatAlertTimestamp(incident.evidence_from_ms)} →
							{formatAlertTimestamp(incident.evidence_to_ms)}
						</dd>
						<dt class="text-(--shell-text-muted)">Rule</dt>
						<dd class="text-(--shell-text-secondary) break-all">
							{incident.rule_id ?? '—'}
							{#if incident.rule_revision !== null}
								(rev {incident.rule_revision})
							{/if}
						</dd>
						{#if incident.acknowledged_by}
							<dt class="text-(--shell-text-muted)">Acknowledged</dt>
							<dd class="text-(--shell-text-secondary)">
								{incident.acknowledged_by} · {formatAlertTimestamp(incident.acknowledged_at)}
							</dd>
						{/if}
						{#if incident.silenced_by}
							<dt class="text-(--shell-text-muted)">Silenced by</dt>
							<dd class="text-(--shell-text-secondary)">{incident.silenced_by}</dd>
						{/if}
					</dl>

					{#if isOperator && incident.status !== 'resolved'}
						{@const current = incident}
						<div class="flex flex-wrap items-center gap-2 pt-1 border-t border-(--shell-line)">
							<Button
								variant="secondary"
								size="sm"
								disabled={busy}
								onclick={async () => {
									busy = true;
									try {
										await onAcknowledge(current);
									} finally {
										busy = false;
									}
								}}
							>
								<Check size={14} />
								Acknowledge
							</Button>
							{#each SILENCE_PICKS as pick (pick.minutes)}
								<Button
									variant="secondary"
									size="sm"
									disabled={busy}
									onclick={async () => {
										busy = true;
										try {
											await onSilence(current, pick.minutes);
										} finally {
											busy = false;
										}
									}}
								>
									<BellOff size={14} />
									Silence {pick.label}
								</Button>
							{/each}
						</div>
					{/if}

					<div>
						<h4 class="text-[10px] font-bold uppercase tracking-wider text-(--shell-text-muted) m-0 mb-2">
							State transitions
						</h4>
						{#if transitions.length === 0}
							<p class="text-xs text-(--shell-text-muted)">No transitions recorded.</p>
						{:else}
							<ol class="list-none m-0 p-0 flex flex-col">
								{#each transitions as transition, index (index)}
									<li class="flex gap-3">
										<div class="flex flex-col items-center">
											<span
												class="w-2 h-2 rounded-full mt-1 shrink-0 {transition.to_state === 'firing'
													? 'bg-danger'
													: transition.to_state === 'resolved'
														? 'bg-success'
														: 'bg-warning'}"
											></span>
											{#if index < transitions.length - 1}
												<span class="w-px flex-1 bg-(--shell-line) my-1"></span>
											{/if}
										</div>
										<div class="pb-4 min-w-0">
											<p class="text-xs font-semibold text-(--shell-text) m-0">
												{transition.from_state ?? 'created'} → {transition.to_state}
											</p>
											<p class="text-[10px] text-(--shell-text-muted) m-0 mt-0.5">
												{formatAlertTimestamp(transition.occurred_at_ms)} · {transition.reason}
											</p>
											{#if transition.measured}
												<p class="text-[10px] text-(--shell-text-secondary) m-0 mt-0.5">
													measured: {transition.measured}
												</p>
											{/if}
										</div>
									</li>
								{/each}
							</ol>
						{/if}
					</div>
				{/if}
			</div>
		</div>
	</div>
{/if}
