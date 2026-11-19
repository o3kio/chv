<script lang="ts">
	import { goto } from '$app/navigation';
	import Button from '#lib/components/primitives/Button.svelte';
	import ArchitectureMetaPanel from '#lib/components/architectures/dashboard/ArchitectureMetaPanel.svelte';
	import StaleVersionBanner from '#lib/components/architectures/dashboard/StaleVersionBanner.svelte';
	import StarterBanner from '#lib/components/architectures/dashboard/StarterBanner.svelte';
	import StarterCloneButton from '#lib/components/architectures/dashboard/StarterCloneButton.svelte';
	import ValidationFindingsPanel from '#lib/components/architectures/dashboard/ValidationFindingsPanel.svelte';
	import FleetCheckPanel from '#lib/components/architectures/dashboard/FleetCheckPanel.svelte';
	import PlanReviewPanel from '#lib/components/architectures/dashboard/PlanReviewPanel.svelte';
	import YamlSidePanel from '#lib/components/architectures/dashboard/YamlSidePanel.svelte';
	import DriftReportPanel from '#lib/components/architectures/drift/DriftReportPanel.svelte';
	import NetboxPanel from '#lib/components/architectures/netbox/NetboxPanel.svelte';
	import Canvas from '#lib/components/architectures/canvas/Canvas.svelte';
	import Inspector from '#lib/components/architectures/inspector/Inspector.svelte';
	import { isStarter, buildCloneNames } from '#lib/architectures/starter.ts';
	import { liveState } from '#lib/stores/live-state.svelte.ts';
	import { architectureStore, StaleVersionError } from '#lib/stores/architecture-store.svelte.ts';
	import { architectureRunsStore } from '#lib/stores/architecture-runs-store.svelte.ts';
	import {
		architectureCanvasStore,
		type GraphPayload
	} from '#lib/stores/architecture-canvas-store.svelte.ts';
	import { architectureDesignerCanvasEnabled } from '#lib/feature-flags.ts';
	import { BFFError } from '#lib/bff/client.ts';
	import type { Architecture, FleetCheckResult, PlanMode, PlanResult, ValidationResult } from '#lib/bff/architectures.ts';
	import type { PageData } from './$types';

	let { data }: { data: PageData } = $props();
	const detail = $derived(data.detail);

	let current = $state<Architecture | null>(null);
	let staleVersion = $state(false);

	// Canvas wiring (Phase 2). The flag defaults ON as of Phase 4 — set
	// PUBLIC_ARCHITECTURE_DESIGNER_CANVAS_DISABLED=1 only as an emergency opt-out.
	const canvasEnabled = architectureDesignerCanvasEnabled();
	let canvasDirty = $state(false);
	let canvasSaving = $state(false);
	let lastHydratedId = $state<string | null>(null);

	// Tab state. The Validation and YAML tabs are Phase 1 additions; the Fleet
	// tab arrives in Phase 3. Overview remains the default so existing
	// playwright tests stay green. The NetBox tab is the issue-239
	// projection panel (PR 7) — mounted only when active, like every other
	// branch, so its lazy-load fires on first activation.
	type Tab = 'overview' | 'yaml' | 'validation' | 'fleet' | 'plan' | 'drift' | 'netbox';
	let activeTab = $state<Tab>('overview');

	// Validation panel state. Findings are NOT persisted server-side; we keep
	// the latest result locally and re-fetch when the user clicks Re-validate
	// or re-enters the tab from a fresh load.
	let validationResult = $state<ValidationResult | null>(null);
	let validating = $state(false);

	// Fleet-check panel state. Same lifecycle as validation: not persisted, so
	// re-running the check on tab activation gives the operator a fresh
	// snapshot. The architecture row's `last_fleet_check_status` is what
	// persists across reloads (refreshed via mutateWithRefresh).
	let fleetResult = $state<FleetCheckResult | null>(null);
	let fleetLoading = $state(false);

	// Plan-review state (Phase 4). Plans are persisted server-side with a 15
	// minute TTL but we don't auto-fetch here — the operator clicks Generate to
	// produce one. State stays local so the tab is always interactive even on
	// a freshly loaded page.
	let planResult = $state<PlanResult | null>(null);
	let planLoading = $state(false);
	let planDiscarding = $state(false);
	let planApplying = $state(false);

	// YAML side panel state. We also lazy-load on first tab activation.
	let yamlContent = $state<string | null>(null);
	let yamlLoading = $state(false);
	let yamlEmptyReason = $state<string | undefined>(undefined);

	// Starter clone state. `isStarterArch` is derived from the current row's
	// `name` + `owner_user_id` (the wire intentionally has no `labels`); see
	// `#lib/architectures/starter.ts`. `cloneError` surfaces inline next to the
	// button on failure rather than logging the user out or navigating away.
	const isStarterArch = $derived(current ? isStarter(current) : false);
	let cloning = $state(false);
	let cloneError = $state<string | null>(null);

	$effect(() => {
		if (detail.state === 'ready') {
			current = detail.architecture;
			staleVersion = false;
			// Seed the YAML panel from the loader payload so a freshly imported
			// topology shows its YAML without requiring a Generate click.
			yamlContent = detail.latestYaml;
			// Hydrate the canvas store from the persisted graph blob, but only
			// on architecture-id transitions — re-hydrating after every save
			// would clobber unsaved local edits.
			if (canvasEnabled && lastHydratedId !== detail.architecture.id) {
				lastHydratedId = detail.architecture.id;
				canvasDirty = false;
				architectureCanvasStore.load(parseGraphBlob(detail.designGraphJson));
			}
		} else {
			current = null;
		}
	});

	const heading = $derived(current ? (current.display_name ?? current.name) : '');

	function handleUpdated(next: Architecture) {
		current = next;
	}

	function handleStaleVersion() {
		staleVersion = true;
	}

	async function handleReload() {
		staleVersion = false;
		await liveState.invalidateAndRefresh({ patterns: ['architectures:'], detailId: current?.id });
	}

	async function handleRevalidate() {
		if (!current || validating) return;
		validating = true;
		try {
			validationResult = await architectureStore.validate(current.id);
		} catch {
			// mutateWithRefresh has already toasted the error.
		} finally {
			validating = false;
		}
	}

	async function handleFleetRefresh() {
		if (!current || fleetLoading) return;
		fleetLoading = true;
		try {
			fleetResult = await architectureStore.checkFleet(current.id);
		} catch {
			// mutateWithRefresh has already toasted the error; keep prior
			// fleetResult so the user does not lose context on a transient
			// inventory failure.
		} finally {
			fleetLoading = false;
		}
	}

	async function handlePlanGenerate(mode: PlanMode) {
		if (!current || planLoading) return;
		planLoading = true;
		try {
			planResult =
				mode === 'destroy'
					? await architectureStore.destroyPlan(current.id)
					: await architectureStore.plan(current.id);
		} catch {
			// mutateWithRefresh has already toasted the error; keep the previous
			// planResult so the operator does not lose context.
		} finally {
			planLoading = false;
		}
	}

	async function handlePlanApply(
		planId: string,
		mode: PlanMode,
		typedName: string,
		acknowledgedWarnings: boolean
	) {
		if (!current || planApplying) return;
		planApplying = true;
		try {
			const confirmation = typedName ? { typed_name: typedName } : {};
			await architectureRunsStore.applyAndNavigate(
				current.id,
				planId,
				confirmation,
				acknowledgedWarnings,
				mode
			);
			// applyAndNavigate already issued goto(); planResult intentionally
			// stays so a back-navigation lands on the prior state.
		} catch {
			// mutateWithRefresh has already toasted the BFF error; staying on
			// this page lets the operator fix typed-name / warnings and retry.
		} finally {
			planApplying = false;
		}
	}

	async function handlePlanDiscard(planId: string) {
		if (planDiscarding) return;
		planDiscarding = true;
		try {
			await architectureStore.discardPlan(planId);
			planResult = null;
		} catch {
			// mutateWithRefresh has already toasted the error.
		} finally {
			planDiscarding = false;
		}
	}

	async function handleGenerateYaml() {
		if (!current || yamlLoading) return;
		yamlLoading = true;
		yamlEmptyReason = undefined;
		try {
			yamlContent = await architectureStore.generateYaml(current.id);
		} catch (err) {
			if (err instanceof BFFError && err.code === 'GRAPH_EMPTY') {
				yamlContent = null;
				yamlEmptyReason = 'This topology has no graph yet — design the topology first (Phase 2 canvas) or import YAML.';
			} else {
				yamlEmptyReason = err instanceof Error ? err.message : 'Failed to generate YAML';
			}
		} finally {
			yamlLoading = false;
		}
	}

	function parseGraphBlob(blob: string | null): GraphPayload | null {
		if (!blob) return null;
		try {
			const parsed = JSON.parse(blob);
			if (parsed && typeof parsed === 'object' && parsed.version === '1.0') {
				return parsed as GraphPayload;
			}
		} catch {
			// fall through — malformed blob loads as empty graph
		}
		return null;
	}

	async function handleClone() {
		if (!current || cloning) return;
		cloning = true;
		cloneError = null;
		try {
			// Short id keeps the cloned slug tidy; the full UUID would dominate
			// dashboard card titles. Collisions on the 8-char fragment are
			// negligible for per-user manual clone flows.
			const shortId = crypto.randomUUID().slice(0, 8);
			const { name, display_name } = buildCloneNames(current, shortId);
			// BFF createArchitecture accepts `design_graph_json` and `latest_yaml`
			// directly (see CreateArchitectureRequest), so a deep clone is a
			// single round-trip — no follow-up updateArchitecture needed.
			const designGraphJson = detail.state === 'ready' ? detail.designGraphJson : null;
			const latestYaml = detail.state === 'ready' ? detail.latestYaml : null;
			const created = await architectureStore.create({
				name,
				display_name,
				description: current.description ?? null,
				environment: current.environment ?? null,
				design_graph_json: designGraphJson,
				latest_yaml: latestYaml
			});
			await goto(`/architectures/${created.id}`);
		} catch (err) {
			cloneError = err instanceof Error ? err.message : 'Failed to clone starter';
		} finally {
			cloning = false;
		}
	}

	function handleCanvasChange() {
		canvasDirty = true;
	}

	async function handleCanvasSave() {
		if (!current || canvasSaving) return;
		canvasSaving = true;
		try {
			const updated = await architectureCanvasStore.persist(
				current.id,
				current.version_number
			);
			current = updated;
			canvasDirty = false;
			// Findings re-fetch so per-node badges reflect the freshly persisted
			// graph. handleRevalidate() already toasts on its own errors.
			await handleRevalidate();
		} catch (err) {
			if (err instanceof StaleVersionError) {
				staleVersion = true;
			}
			// Any other error has already been toasted by mutateWithRefresh.
		} finally {
			canvasSaving = false;
		}
	}
</script>

<svelte:head>
	<title>
		{current ? `${heading} · Architecture` : 'Architecture · CellHV'}
	</title>
</svelte:head>

<div class="page" data-testid="architecture-detail-page">
	{#if detail.state === 'error' || !current}
		<div class="error-banner" role="alert">
			<strong>Could not load architecture {detail.state === 'error' ? detail.id : ''}.</strong>
			<span>
				{detail.state === 'error' ? detail.errorMessage : 'No data returned by the server.'}
			</span>
			<div>
				<Button variant="secondary" size="sm" onclick={() => goto('/architectures')}>
					Back to list
				</Button>
			</div>
		</div>
	{:else}
		<header class="page-header">
			<div>
				<button
					type="button"
					class="back-link"
					onclick={() => goto('/architectures')}
					aria-label="Back to architectures"
				>
					← Architectures
				</button>
				<h1 class="page-title" data-testid="architecture-name">{heading}</h1>
				<p class="page-subtitle">
					Phase 1 detail view. The visual designer arrives in Phase 2.
				</p>
			</div>
			<div class="page-header-actions">
				{#if isStarterArch}
					<StarterCloneButton {cloning} onClone={handleClone} />
				{/if}
				<a
					class="runs-link"
					href={`/architectures/${current.id}/runs`}
					data-testid="architecture-runs-link"
					aria-label={`View runs for ${heading}`}
				>
					View runs →
				</a>
			</div>
		</header>

		{#if cloneError}
			<div class="error-banner" role="alert" data-testid="clone-starter-error">
				<strong>Could not clone starter.</strong>
				<span>{cloneError}</span>
			</div>
		{/if}

		{#if staleVersion}
			<StaleVersionBanner onReload={handleReload} />
		{/if}

		{#if isStarterArch}
			<StarterBanner />
		{/if}

		<ArchitectureMetaPanel
			architecture={current}
			onUpdated={handleUpdated}
			onStaleVersion={handleStaleVersion}
		/>

		<div class="tablist" role="tablist" aria-label="Architecture views">
			<button
				type="button"
				role="tab"
				aria-selected={activeTab === 'overview'}
				aria-controls="tab-panel-overview"
				class="tab"
				class:tab-active={activeTab === 'overview'}
				onclick={() => (activeTab = 'overview')}
				data-testid="tab-overview"
			>
				Overview
			</button>
			<button
				type="button"
				role="tab"
				aria-selected={activeTab === 'yaml'}
				aria-controls="tab-panel-yaml"
				class="tab"
				class:tab-active={activeTab === 'yaml'}
				onclick={() => (activeTab = 'yaml')}
				data-testid="tab-yaml"
			>
				YAML
			</button>
			<button
				type="button"
				role="tab"
				aria-selected={activeTab === 'validation'}
				aria-controls="tab-panel-validation"
				class="tab"
				class:tab-active={activeTab === 'validation'}
				onclick={() => (activeTab = 'validation')}
				data-testid="tab-validation"
			>
				Validation
			</button>
			<button
				type="button"
				role="tab"
				aria-selected={activeTab === 'fleet'}
				aria-controls="tab-panel-fleet"
				class="tab"
				class:tab-active={activeTab === 'fleet'}
				onclick={() => (activeTab = 'fleet')}
				data-testid="tab-fleet"
			>
				Fleet check
			</button>
			<button
				type="button"
				role="tab"
				aria-selected={activeTab === 'plan'}
				aria-controls="tab-panel-plan"
				class="tab"
				class:tab-active={activeTab === 'plan'}
				onclick={() => (activeTab = 'plan')}
				data-testid="tab-plan"
			>
				Plan
			</button>
			<button
				type="button"
				role="tab"
				aria-selected={activeTab === 'drift'}
				aria-controls="tab-panel-drift"
				class="tab"
				class:tab-active={activeTab === 'drift'}
				onclick={() => (activeTab = 'drift')}
				data-testid="tab-drift"
			>
				Drift
			</button>
			<button
				type="button"
				role="tab"
				id="tab-netbox"
				aria-selected={activeTab === 'netbox'}
				aria-controls="tab-panel-netbox"
				class="tab"
				class:tab-active={activeTab === 'netbox'}
				onclick={() => (activeTab = 'netbox')}
				data-testid="tab-netbox"
			>
				NetBox
			</button>
		</div>

		{#if activeTab === 'overview'}
			{#if canvasEnabled && current.status !== 'archived'}
				<div
					id="tab-panel-overview"
					class="canvas-shell"
					role="tabpanel"
					aria-labelledby="tab-overview"
					data-testid="canvas-shell"
				>
					<div class="canvas-toolbar" data-testid="canvas-toolbar">
						<div class="toolbar-status">
							{#if architectureCanvasStore.dirty || canvasDirty}
								<span
									class="dirty-dot"
									data-testid="canvas-dirty-indicator"
									role="img"
									aria-label="Unsaved canvas changes"
									title="Unsaved canvas changes"
								></span>
								<span class="toolbar-status-text">Unsaved changes</span>
							{:else}
								<span class="toolbar-status-text">All changes saved</span>
							{/if}
						</div>
						<div class="toolbar-actions">
							<Button
								variant="secondary"
								size="sm"
								onclick={handleRevalidate}
								disabled={validating}
								data-testid="canvas-validate-button"
							>
								{validating ? 'Validating…' : 'Validate'}
							</Button>
							<Button
								variant="primary"
								size="sm"
								onclick={handleCanvasSave}
								disabled={canvasSaving || !(architectureCanvasStore.dirty || canvasDirty)}
								data-testid="canvas-save-button"
							>
								{canvasSaving ? 'Saving…' : 'Save canvas'}
							</Button>
						</div>
					</div>
					<div class="canvas-grid">
						<Canvas
							findings={validationResult?.findings ?? []}
							onChange={handleCanvasChange}
						/>
						<Inspector />
					</div>
				</div>
			{:else if !canvasEnabled}
				<div
					id="tab-panel-overview"
					class="canvas-placeholder"
					role="tabpanel"
					aria-labelledby="tab-overview"
					aria-label="Designer canvas disabled"
					data-testid="canvas-disabled-banner"
				>
					<div class="placeholder-title">Designer canvas disabled</div>
					<p class="placeholder-text">
						The Svelte Flow canvas has been disabled by an operator
						(<code>PUBLIC_ARCHITECTURE_DESIGNER_CANVAS_DISABLED=1</code>). The
						YAML and Validation tabs remain available; unset the env var or
						restart the BFF to re-enable the canvas.
					</p>
				</div>
			{:else}
				<div
					id="tab-panel-overview"
					class="canvas-placeholder"
					role="tabpanel"
					aria-labelledby="tab-overview"
					aria-label="Architecture archived — canvas read-only"
					data-testid="canvas-archived-banner"
				>
					<div class="placeholder-title">Architecture archived</div>
					<p class="placeholder-text">
						This architecture has been archived and is read-only. Use the YAML
						tab to inspect the saved topology, or restore the architecture to
						resume editing.
					</p>
				</div>
			{/if}
		{:else if activeTab === 'yaml'}
			<div id="tab-panel-yaml" role="tabpanel" aria-labelledby="tab-yaml">
				<YamlSidePanel
					yaml={yamlContent}
					loading={yamlLoading}
					emptyReason={yamlEmptyReason}
					onGenerate={handleGenerateYaml}
				/>
			</div>
		{:else if activeTab === 'validation'}
			<div id="tab-panel-validation" role="tabpanel" aria-labelledby="tab-validation">
				<ValidationFindingsPanel
					result={validationResult}
					loading={validating}
					onRevalidate={handleRevalidate}
				/>
			</div>
		{:else if activeTab === 'fleet'}
			<div id="tab-panel-fleet" role="tabpanel" aria-labelledby="tab-fleet">
				<FleetCheckPanel
					result={fleetResult}
					loading={fleetLoading}
					onRefresh={handleFleetRefresh}
				/>
			</div>
		{:else if activeTab === 'plan'}
			<div id="tab-panel-plan" role="tabpanel" aria-labelledby="tab-plan">
				<PlanReviewPanel
					architecture={current}
					planResult={planResult}
					loading={planLoading}
					discarding={planDiscarding}
					applying={planApplying}
					onGenerate={handlePlanGenerate}
					onDiscard={handlePlanDiscard}
					onApply={handlePlanApply}
				/>
			</div>
		{:else if activeTab === 'drift'}
			<div id="tab-panel-drift" role="tabpanel" aria-labelledby="tab-drift">
				<DriftReportPanel architectureId={current.id} />
			</div>
		{:else if activeTab === 'netbox'}
			<div id="tab-panel-netbox" role="tabpanel" aria-labelledby="tab-netbox">
				<NetboxPanel architecture={current} onStaleVersion={handleStaleVersion} />
			</div>
		{/if}
	{/if}
</div>

<style>
	.page {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.page-header {
		display: flex;
		flex-direction: row;
		align-items: flex-start;
		justify-content: space-between;
		gap: 1rem;
		flex-wrap: wrap;
	}

	.page-header-actions {
		display: flex;
		align-items: center;
		gap: 0.5rem;
	}

	.runs-link {
		font-size: var(--chv-text-sm);
		color: var(--color-primary);
		text-decoration: none;
		padding: 0.25rem 0.5rem;
		border-radius: var(--radius-xs);
	}

	.runs-link:hover,
	.runs-link:focus-visible {
		text-decoration: underline;
		outline: 2px solid var(--color-primary);
		outline-offset: 2px;
	}

	.back-link {
		background: none;
		border: none;
		padding: 0;
		font-size: var(--chv-text-xs);
		color: var(--chv-color-neutral-500);
		cursor: pointer;
		align-self: flex-start;
	}

	.back-link:hover,
	.back-link:focus-visible {
		color: var(--color-primary);
		outline: none;
	}

	.page-title {
		font-size: var(--chv-text-lg);
		font-weight: 700;
		margin: 0;
	}

	.page-subtitle {
		margin: 0;
		font-size: var(--chv-text-sm);
		color: var(--chv-color-neutral-600);
	}

	.tablist {
		display: flex;
		gap: 0.25rem;
		border-bottom: 1px solid var(--chv-color-neutral-200);
	}

	.tab {
		appearance: none;
		background: none;
		border: none;
		padding: 0.5rem 0.85rem;
		font-size: var(--chv-text-sm);
		color: var(--chv-color-neutral-600);
		cursor: pointer;
		border-bottom: 2px solid transparent;
		margin-bottom: -1px;
	}

	.tab:hover {
		color: var(--chv-color-neutral-900);
	}

	.tab:focus-visible {
		outline: 2px solid var(--color-primary);
		outline-offset: 2px;
	}

	.tab-active {
		color: var(--color-primary);
		border-bottom-color: var(--color-primary);
		font-weight: 600;
	}

	.canvas-placeholder {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		padding: 2rem;
		background: var(--chv-color-neutral-50, #f8fafc);
		border: 1px dashed var(--chv-color-neutral-300);
		border-radius: var(--radius-sm);
		text-align: center;
	}

	.canvas-shell {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}

	.canvas-toolbar {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 0.5rem;
		padding: 0.5rem 0.75rem;
		background: var(--chv-color-neutral-50, #f8fafc);
		border: 1px solid var(--chv-color-neutral-200);
		border-radius: var(--radius-sm);
	}

	.toolbar-status {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		font-size: var(--chv-text-xs);
		color: var(--chv-color-neutral-600);
	}

	.toolbar-actions {
		display: flex;
		gap: 0.5rem;
	}

	.dirty-dot {
		display: inline-block;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--color-warning, #f59e0b);
	}

	.canvas-grid {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(280px, 320px);
		gap: 0.75rem;
		min-height: 520px;
	}

	.placeholder-title {
		font-size: var(--chv-text-sm);
		font-weight: 600;
		color: var(--chv-color-neutral-700);
	}

	.placeholder-text {
		margin: 0 auto;
		max-width: 520px;
		font-size: 12px;
		color: var(--chv-color-neutral-500);
		line-height: 1.5;
	}

	.error-banner {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		padding: 0.75rem 1rem;
		background: rgba(220, 38, 38, 0.08);
		border: 1px solid rgba(220, 38, 38, 0.4);
		border-radius: var(--radius-xs);
		color: rgb(153, 27, 27);
		font-size: var(--chv-text-sm);
	}
</style>
