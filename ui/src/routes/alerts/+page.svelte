<script lang="ts">
	import { untrack } from 'svelte';
	import { Calendar, BellRing } from 'lucide-svelte';
	import PageHeaderWithAction from '#lib/components/shell/PageHeaderWithAction.svelte';
	import CompactMetricCard from '#lib/components/shared/CompactMetricCard.svelte';
	import { getPageDefinition } from '#lib/shell/app-shell.ts';
	import { getStoredRole, getStoredToken } from '#lib/api/client.ts';
	import {
		listDeliveries,
		listIncidents,
		listRules,
		type AlertDelivery,
		type AlertRule,
		type Incident
	} from '#lib/bff/alerting.ts';
	import IncidentsTab from '#lib/components/alerts/IncidentsTab.svelte';
	import RulesTab from '#lib/components/alerts/RulesTab.svelte';
	import RecentDeliveriesCard from '#lib/components/alerts/RecentDeliveriesCard.svelte';
	import type { PageData } from './$types';

	let { data }: { data: PageData } = $props();

	const page = getPageDefinition('/alerts');

	// Rule mutations, acknowledgment and silence are operator-gated at
	// the BFF; the UI hides them from viewers (role tiers are
	// fleet-scoped in v1 — the recorded limitation).
	const isOperator = $derived(
		['operator', 'admin'].includes(getStoredRole() ?? 'viewer')
	);

	// Local editable copies of the load data: mutations reload through
	// the BFF and write back here, so the initial capture is exactly
	// what we want (svelte-ignore state_referenced_locally covers the
	// intentional one-time initialization below).
	// svelte-ignore state_referenced_locally
	let activeTab = $state<'incidents' | 'rules'>(data.tab);

	// A deep link / navigation reruns load; follow it when the URL's
	// tab differs from the local selection. `untrack` keeps this from
	// fighting the local tab clicks (which only replaceState).
	$effect(() => {
		const urlTab = data.tab;
		const current = untrack(() => activeTab);
		if (urlTab !== current) activeTab = urlTab;
	});

	// svelte-ignore state_referenced_locally
	let incidents = $state<Incident[]>(data.incidents);
	// svelte-ignore state_referenced_locally
	let incidentTotal = $state(data.incidentTotal);
	// svelte-ignore state_referenced_locally
	let incidentsError = $state(data.incidentsError);
	// svelte-ignore state_referenced_locally
	let rules = $state<AlertRule[]>(data.rules);
	// svelte-ignore state_referenced_locally
	let ruleTotal = $state(data.ruleTotal);
	// svelte-ignore state_referenced_locally
	let rulesError = $state(data.rulesError);
	// svelte-ignore state_referenced_locally
	let deliveries = $state<AlertDelivery[]>(data.deliveries);
	let loading = $state(false);

	// Tab changes reflected in the URL so deep links land on the right tab.
	$effect(() => {
		const current = activeTab;
		const url = new URL(window.location.href);
		if (url.searchParams.get('tab') !== current) {
			url.searchParams.set('tab', current);
			window.history.replaceState({}, '', url);
		}
	});

	async function reloadIncidents(includeResolved: boolean): Promise<void> {
		loading = true;
		try {
			const response = await listIncidents(
				{ include_resolved: includeResolved, limit: 100 },
				getStoredToken() ?? undefined
			);
			incidents = response.incidents;
			incidentTotal = response.total;
			incidentsError = false;
		} catch {
			incidentsError = true;
		} finally {
			loading = false;
		}
	}

	async function reloadRules(): Promise<void> {
		loading = true;
		try {
			const response = await listRules({ limit: 100 }, getStoredToken() ?? undefined);
			rules = response.rules;
			ruleTotal = response.total;
			rulesError = false;
		} catch {
			rulesError = true;
		} finally {
			loading = false;
		}
	}

	async function reloadDeliveries(): Promise<void> {
		try {
			const response = await listDeliveries({ limit: 10 }, getStoredToken() ?? undefined);
			deliveries = response.deliveries;
		} catch {
			// Delivery audit is best-effort; a failure leaves the last
			// rows in place rather than blanking the card.
		}
	}

	const firingCount = $derived(incidents.filter((i) => i.status === 'firing').length);
	const pendingCount = $derived(incidents.filter((i) => i.status === 'pending').length);
	const enabledRuleCount = $derived(rules.filter((r) => r.enabled).length);
</script>

<div class="inventory-page">
	<PageHeaderWithAction page={page} />

	<div class="inventory-metrics">
		<CompactMetricCard label="Firing Incidents" value={firingCount} color={firingCount > 0 ? 'danger' : 'neutral'} />
		<CompactMetricCard label="Pending Incidents" value={pendingCount} color={pendingCount > 0 ? 'warning' : 'neutral'} />
		<CompactMetricCard label="Active Rules" value={enabledRuleCount} color="primary" />
		<CompactMetricCard label="Rule Registry" value={ruleTotal} color="neutral" />
	</div>

	<div class="inventory-controls-strip">
		<div class="tab-registry" role="tablist" aria-label="Alert views">
			<button
				type="button"
				role="tab"
				id="tab-incidents"
				aria-selected={activeTab === 'incidents'}
				aria-controls="panel-alerts"
				class="tab-btn"
				class:is-active={activeTab === 'incidents'}
				onclick={() => (activeTab = 'incidents')}
			>
				<BellRing size={12} />
				<span>INCIDENTS</span>
			</button>
			<button
				type="button"
				role="tab"
				id="tab-rules"
				aria-selected={activeTab === 'rules'}
				aria-controls="panel-alerts"
				class="tab-btn"
				class:is-active={activeTab === 'rules'}
				onclick={() => (activeTab = 'rules')}
			>
				<Calendar size={12} />
				<span>RULES</span>
			</button>
		</div>
	</div>

	<div class="inventory-main" id="panel-alerts" role="tabpanel" aria-labelledby="tab-{activeTab}">
		<div class="tab-panel">
			{#if activeTab === 'incidents'}
				<IncidentsTab
					incidents={incidents}
					total={incidentTotal}
					{loading}
					error={incidentsError}
					{isOperator}
					onReload={reloadIncidents}
				/>
				<RecentDeliveriesCard {deliveries} />
			{:else}
				<RulesTab
					rules={rules}
					total={ruleTotal}
					{loading}
					error={rulesError}
					{isOperator}
					onReload={() => Promise.all([reloadRules(), reloadDeliveries()]).then(() => undefined)}
				/>
			{/if}
		</div>
	</div>
</div>

<style>
	.inventory-page {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}

	.inventory-metrics {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
		gap: 0.75rem;
	}

	.inventory-controls-strip {
		background: var(--bg-surface);
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-xs);
		padding: 0 0.5rem;
	}

	.tab-registry {
		display: flex;
		gap: 1.5rem;
	}

	.tab-btn {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		padding: 0.75rem 0.25rem;
		background: transparent;
		border: none;
		border-bottom: 2px solid transparent;
		font-size: 10px;
		font-weight: 800;
		color: var(--chv-color-neutral-400);
		cursor: pointer;
		letter-spacing: 0.05em;
	}

	.tab-btn:hover {
		color: var(--chv-color-neutral-600);
	}

	.tab-btn.is-active {
		color: var(--color-primary);
		border-bottom-color: var(--color-primary);
	}

	.tab-panel {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}
</style>
