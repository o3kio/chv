<script lang="ts">
	import { onMount } from 'svelte';
	import { browser } from '$app/env';
	import type { PageData } from './$types';
	import './+page.css';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import TaskTimeline from '#lib/components/shell/TaskTimeline.svelte';
	import SeverityShield from '#lib/components/shell/SeverityShield.svelte';
	import ErrorState from '#lib/components/shell/ErrorState.svelte';
	import EmptyInfrastructureState from '#lib/components/shell/EmptyInfrastructureState.svelte';
	import LoadingState from '#lib/components/shell/LoadingState.svelte';
	import TopologyCanvas from '#lib/components/shared/TopologyCanvas.svelte';
	import CompactMetricCard from '#lib/components/shared/CompactMetricCard.svelte';
	import MonitoringHealthCard from '#lib/components/monitoring/MonitoringHealthCard.svelte';
	import {
		Activity,
		AlertCircle,
		Minus,
		Plus,
		Server,
		ShieldCheck,
		X,
		Zap
	} from 'lucide-svelte';
	import { inventory } from '#lib/stores/inventory.svelte.ts';
	import { dashboard } from '#lib/stores/dashboard.svelte.ts';
	import {
		dashboardPanels,
		defaultPanelState,
		dashboardPanelStorageKey,
		formatRecentTasks,
		getActiveTopologyResourceIds,
		buildFleetBriefing,
		buildPressureCards,
		type PanelId
	} from '#lib/helpers/dashboard.ts';

	let { data }: { data: PageData } = $props();

	$effect(() => {
		dashboard.overview = data.overview;
	});

	let panelVisible = $state<Record<PanelId, boolean>>({ ...defaultPanelState });
	let panelCollapsed = $state<Record<PanelId, boolean>>({
		briefing: false,
		attention: false,
		pipeline: false,
		capacity: false
	});
	let panelPrefsLoaded = $state(false);

	const overview = $derived(dashboard.overview);
	const recentTasks = $derived(formatRecentTasks(overview.recent_tasks));
	const activeTopologyResourceIds = $derived(getActiveTopologyResourceIds(recentTasks));
	const fleetBriefing = $derived(buildFleetBriefing(overview, inventory.nodes, inventory.vms));
	const pressureCards = $derived(buildPressureCards(overview));
	const cpuUsage = $derived(overview.cpu_usage_percent);
	const memoryUsage = $derived(overview.memory_usage_percent);

	const hasCapacityPanel = $derived(panelVisible.capacity && pressureCards.length > 0);
	const hasBriefingRow = $derived(panelVisible.briefing || panelVisible.attention);
	const hasRailPanels = $derived(panelVisible.pipeline || hasCapacityPanel);
	const compactRail = $derived(
		hasRailPanels &&
			(!panelVisible.pipeline || panelCollapsed.pipeline) &&
			(!hasCapacityPanel || panelCollapsed.capacity)
	);
	const availablePanels = $derived(
		dashboardPanels.filter((panel) => panel.id !== 'capacity' || pressureCards.length > 0)
	);

	onMount(() => {
		dashboard.startPolling();

		try {
			const stored = localStorage.getItem(dashboardPanelStorageKey);
			if (stored) {
				const parsed = JSON.parse(stored) as {
					visible?: Partial<Record<PanelId, boolean>>;
					collapsed?: Partial<Record<PanelId, boolean>>;
				};
				panelVisible = { ...panelVisible, ...parsed.visible };
				panelCollapsed = { ...panelCollapsed, ...parsed.collapsed };
			}
		} catch {
			// Ignore malformed local dashboard preferences.
		} finally {
			panelPrefsLoaded = true;
		}

		return () => {
			dashboard.stopPolling();
		};
	});

	$effect(() => {
		const prefs = {
			visible: { ...panelVisible },
			collapsed: { ...panelCollapsed }
		};
		if (!browser || !panelPrefsLoaded) return;
		localStorage.setItem(dashboardPanelStorageKey, JSON.stringify(prefs));
	});

	function togglePanelVisibility(id: PanelId) {
		panelVisible[id] = !panelVisible[id];
		if (panelVisible[id]) panelCollapsed[id] = false;
	}

	function togglePanelCollapsed(id: PanelId) {
		panelCollapsed[id] = !panelCollapsed[id];
	}

	function hidePanel(id: PanelId) {
		panelVisible[id] = false;
	}
</script>

<div class="cockpit-dashboard">
	{#if overview.state === 'error'}
		<ErrorState title="Telemetry Failure" description="Fleet-wide health signals are currently unreachable." />
	{:else if overview.state === 'loading' || inventory.isLoading}
		<LoadingState title="Indexing topology..." />
	{:else if overview.state === 'empty' && inventory.nodes.length === 0}
		<EmptyInfrastructureState
			title="Empty Fleet"
			description="No clusters or nodes are currently indexed."
			hint="Enroll infrastructure to see real-time topology."
		/>
	{:else}
		<div class="cockpit-layout">
			<div class="cockpit-metrics">
				<CompactMetricCard
					label="Managed Nodes"
					value={inventory.nodes.length}
					color="primary"
				/>
				<CompactMetricCard
					label="Running Workloads"
					value={inventory.vms.filter(v => v.actual_state === 'running').length}
					unit={`/ ${inventory.vms.length}`}
					color="accent"
				/>
				{#if cpuUsage !== undefined}
					<CompactMetricCard
						label="Fleet CPU"
						value={Math.round(cpuUsage)}
						unit="%"
						color={cpuUsage > 80 ? 'danger' : 'primary'}
					/>
				{:else}
					<CompactMetricCard label="Fleet CPU" value="—" color="neutral" />
				{/if}
				{#if memoryUsage !== undefined}
					<CompactMetricCard
						label="Fleet memory"
						value={Math.round(memoryUsage)}
						unit="%"
						color={memoryUsage > 85 ? 'danger' : 'primary'}
					/>
				{:else}
					<CompactMetricCard label="Fleet memory" value="—" color="neutral" />
				{/if}
			</div>

			<div class="dashboard-panel-bar" aria-label="Dashboard panel visibility">
				<span>Dashboard panels</span>
				{#each availablePanels as panel}
					<button
						type="button"
						class:dashboard-panel-toggle--active={panelVisible[panel.id]}
						aria-pressed={panelVisible[panel.id]}
						onclick={() => togglePanelVisibility(panel.id)}
					>
						{panel.label}
					</button>
				{/each}
			</div>

			{#if hasBriefingRow}
				<div class="cockpit-briefing-grid" class:cockpit-briefing-grid--single={!(panelVisible.briefing && panelVisible.attention)}>
					{#if panelVisible.briefing}
						<SectionCard title="Fleet Briefing" icon={ShieldCheck} badgeLabel="Shift View" collapsed={panelCollapsed.briefing}>
							{#snippet actions()}
								<button class="panel-icon-button" type="button" aria-label={panelCollapsed.briefing ? 'Expand Fleet Briefing' : 'Minimize Fleet Briefing'} onclick={() => togglePanelCollapsed('briefing')}>
									{#if panelCollapsed.briefing}<Plus size={12} />{:else}<Minus size={12} />{/if}
								</button>
								<button class="panel-icon-button" type="button" aria-label="Remove Fleet Briefing from dashboard" onclick={() => hidePanel('briefing')}>
									<X size={12} />
								</button>
							{/snippet}
							<div class="briefing-grid">
								{#each fleetBriefing as item}
									<article class="briefing-card">
										<p class="briefing-label">{item.label}</p>
										<p class="briefing-value">{item.value}</p>
										<p class="briefing-note">{item.note}</p>
									</article>
								{/each}
							</div>
						</SectionCard>
					{/if}

					{#if panelVisible.attention}
						<SectionCard
							title="Immediate Attention"
							icon={AlertCircle}
							badgeLabel={overview.unresolved_alerts > 0 ? String(overview.unresolved_alerts) : 'Clear'}
							badgeTone={overview.unresolved_alerts > 0 ? 'warning' : 'healthy'}
							collapsed={panelCollapsed.attention}
						>
							{#snippet actions()}
								<button class="panel-icon-button" type="button" aria-label={panelCollapsed.attention ? 'Expand Immediate Attention' : 'Minimize Immediate Attention'} onclick={() => togglePanelCollapsed('attention')}>
									{#if panelCollapsed.attention}<Plus size={12} />{:else}<Minus size={12} />{/if}
								</button>
								<button class="panel-icon-button" type="button" aria-label="Remove Immediate Attention from dashboard" onclick={() => hidePanel('attention')}>
									<X size={12} />
								</button>
							{/snippet}
							<ul class="attention-list">
								{#each overview.alerts.slice(0, 4) as alert}
									<li class="attention-item">
										<div class="attention-item__header">
											<SeverityShield severity={alert.severity} />
											<span class="attention-scope">{alert.scope}</span>
										</div>
										<p>{alert.summary}</p>
									</li>
								{/each}
								{#if overview.alerts.length === 0}
									<li class="attention-item attention-item--quiet">
										<Server size={15} />
										<div>
											<p>No alerts reported across the indexed fleet.</p>
											<span>No active incidents are crowding the queue.</span>
										</div>
									</li>
								{/if}
							</ul>
						</SectionCard>
					{/if}
				</div>
			{/if}

			<div class="cockpit-workspace" class:cockpit-workspace--wide={!hasRailPanels} class:cockpit-workspace--compact-rail={compactRail}>
				<section class="cockpit-topology">
					<TopologyCanvas highlightedResourceIds={activeTopologyResourceIds} />
				</section>

				{#if hasRailPanels}
					<aside class="cockpit-rail">
						{#if panelVisible.pipeline}
							<SectionCard title="Operation Pipeline" icon={Activity} badgeLabel="Live" collapsed={panelCollapsed.pipeline}>
								{#snippet actions()}
									<button class="panel-icon-button" type="button" aria-label={panelCollapsed.pipeline ? 'Expand Operation Pipeline' : 'Minimize Operation Pipeline'} onclick={() => togglePanelCollapsed('pipeline')}>
										{#if panelCollapsed.pipeline}<Plus size={12} />{:else}<Minus size={12} />{/if}
									</button>
									<button class="panel-icon-button" type="button" aria-label="Remove Operation Pipeline from dashboard" onclick={() => hidePanel('pipeline')}>
										<X size={12} />
									</button>
								{/snippet}
								<TaskTimeline tasks={recentTasks.slice(0, 4)} />
							</SectionCard>
						{/if}

						{#if hasCapacityPanel}
							<SectionCard title="Capacity Pressure" icon={Zap} collapsed={panelCollapsed.capacity}>
								{#snippet actions()}
									<button class="panel-icon-button" type="button" aria-label={panelCollapsed.capacity ? 'Expand Capacity Pressure' : 'Minimize Capacity Pressure'} onclick={() => togglePanelCollapsed('capacity')}>
										{#if panelCollapsed.capacity}<Plus size={12} />{:else}<Minus size={12} />{/if}
									</button>
									<button class="panel-icon-button" type="button" aria-label="Remove Capacity Pressure from dashboard" onclick={() => hidePanel('capacity')}>
										<X size={12} />
									</button>
								{/snippet}
								<div class="capacity-preview">
									{#each pressureCards as item}
										<div class="cap-item">
											<div class="cap-header">
												<span>{item.label}</span>
												<span>{item.value} · {item.state}</span>
											</div>
											<div class="cap-bar" aria-label="{item.label}: {item.value}, {item.state}">
												<div class="cap-fill" class:cap-fill--warm={item.state === 'Warm'} class:cap-fill--pressure={item.state === 'Pressure'} class:cap-fill--critical={item.state === 'Critical'} style={`width: ${item.width}%`}></div>
											</div>
										</div>
									{/each}
								</div>
							</SectionCard>
						{/if}

						<MonitoringHealthCard />
					</aside>
				{/if}
			</div>
		</div>
	{/if}
</div>
