<script lang="ts">
	import Button from '#lib/components/primitives/Button.svelte';
	import NetboxConfigForm from './NetboxConfigForm.svelte';
	import NetboxDryRunTable from './NetboxDryRunTable.svelte';
	import NetboxRunHistory from './NetboxRunHistory.svelte';
	import NetboxExportButton from './NetboxExportButton.svelte';
	import NetboxErrorBanner from './NetboxErrorBanner.svelte';
	import { netboxDryRunErrorText, netboxExportErrorText, netboxRetryErrorText } from './types.ts';
	import {
		architectureNetboxStore,
		type NetboxConfigDraft
	} from '#lib/stores/architecture-netbox-store.svelte.ts';
	import { StaleVersionError } from '#lib/stores/architecture-store.svelte.ts';
	import type { Architecture } from '#lib/bff/architectures.ts';

	/**
	 * NetBox projection tab body: config form + dry-run section + run
	 * history, composing the subcomponents. All BFF interaction lives in
	 * the netbox store; this panel only wires callbacks and renders
	 * state (mirrors DriftReportPanel's lazy-load-on-activation shape —
	 * the page only mounts the active tab branch, so first activation
	 * equals first mount).
	 *
	 * `onStaleVersion` re-raises the page's existing stale-version
	 * banner when a config save hits a 409 `PLAN_EXPIRED` conflict.
	 */

	interface Props {
		architecture: Architecture;
		onStaleVersion: () => void;
	}

	let { architecture, onStaleVersion }: Props = $props();

	// Lazy-load on first activation, keyed on the architecture id so
	// navigating between architectures resets and re-fetches (same
	// pattern + rationale as DriftReportPanel — see the comment there
	// for why there is deliberately no unmount cleanup racing the
	// in-flight fetch).
	let lastLoadedId: string | null = null;

	$effect(() => {
		const id = architecture.id;
		if (id && id !== lastLoadedId) {
			if (lastLoadedId !== null) {
				architectureNetboxStore.reset();
			}
			lastLoadedId = id;
			void architectureNetboxStore.loadConfig(id);
			void architectureNetboxStore.loadRuns(id);
		}
	});

	const config = $derived(architectureNetboxStore.config);
	const configLoading = $derived(architectureNetboxStore.configLoading);
	const configError = $derived(architectureNetboxStore.configError);
	const dryRunPlan = $derived(architectureNetboxStore.dryRunPlan);
	const dryRunLoading = $derived(architectureNetboxStore.dryRunLoading);
	const dryRunError = $derived(architectureNetboxStore.dryRunError);
	const runs = $derived(architectureNetboxStore.runs);
	const runsLoading = $derived(architectureNetboxStore.runsLoading);
	const runsError = $derived(architectureNetboxStore.runsError);
	const currentRun = $derived(architectureNetboxStore.currentRun);
	const runLoading = $derived(architectureNetboxStore.runLoading);
	const exporting = $derived(architectureNetboxStore.exporting);
	const exportError = $derived(architectureNetboxStore.exportError);
	const retrying = $derived(architectureNetboxStore.retrying);
	const retryError = $derived(architectureNetboxStore.retryError);

	// The store exposes no config-saving flag (its mutation path is
	// fire-and-forget for the flags); the panel owns the button state.
	let savingConfig = $state(false);
	let deletingConfig = $state(false);

	async function handleSaveConfig(draft: NetboxConfigDraft) {
		savingConfig = true;
		try {
			await architectureNetboxStore.saveConfig(architecture.id, architecture.version_number, draft);
		} catch (err) {
			if (err instanceof StaleVersionError) {
				// Raise the page's banner; rethrow so the form keeps the
				// operator's drafts (minus the cleared token).
				onStaleVersion();
			}
			// Every other failure has already been toasted by
			// mutateWithRefresh.
			throw err;
		} finally {
			savingConfig = false;
		}
	}

	async function handleDeleteConfig() {
		deletingConfig = true;
		try {
			await architectureNetboxStore.deleteConfig(architecture.id);
		} catch {
			// Toasted by mutateWithRefresh.
		} finally {
			deletingConfig = false;
		}
	}

	async function handleDryRun() {
		await architectureNetboxStore.runDryRun(architecture.id);
	}

	async function handleExport() {
		await architectureNetboxStore.exportNow(architecture.id);
	}

	function handleSelectRun(runId: string) {
		void architectureNetboxStore.loadRun(architecture.id, runId);
	}

	async function handleRetry(runId: string) {
		try {
			await architectureNetboxStore.retryRun(architecture.id, runId);
		} catch {
			// PROJECTION_RUN_NOT_RETRYABLE / NETBOX_RUN_ACTIVE (or
			// transport) — toasted by mutateWithRefresh and surfaced
			// inline by the retry banner below; the history keeps the
			// failed row.
		}
	}
</script>

<section class="panel" aria-label="NetBox projection" data-testid="netbox-panel">
	<header class="panel-header">
		<div class="header-left">
			<h2 class="panel-title">NetBox projection</h2>
			<span class="panel-sub">
				Projects the most recently <strong>applied</strong> topology into NetBox as a
				downstream, idempotent copy.
			</span>
		</div>
		<div class="header-right">
			<NetboxExportButton
				architectureName={architecture.display_name ?? architecture.name}
				environment={architecture.environment}
				exporting={exporting}
				disabled={!config}
				onExport={handleExport}
			/>
		</div>
	</header>

	{#if exportError}
		<NetboxErrorBanner
			heading="Export failed."
			message={netboxExportErrorText(exportError)}
			testId="netbox-export-error"
			code={exportError.code}
		/>
	{/if}

	<section class="section" aria-labelledby="netbox-config-heading">
		<h3 id="netbox-config-heading" class="section-title">Configuration</h3>
		{#if configLoading && !config}
			<div class="hint" role="status" data-testid="netbox-config-loading">Loading NetBox config…</div>
		{:else}
			{#if configError}
				<NetboxErrorBanner
					heading="Could not load NetBox config."
					message={configError}
					testId="netbox-config-error-banner"
				/>
			{/if}
			<NetboxConfigForm
				architectureId={architecture.id}
				{config}
				expectedVersion={architecture.version_number}
				saving={savingConfig}
				deleting={deletingConfig}
				onSave={handleSaveConfig}
				onDelete={handleDeleteConfig}
			/>
		{/if}
	</section>

	<section class="section" aria-labelledby="netbox-dry-run-heading">
		<div class="section-header">
			<h3 id="netbox-dry-run-heading" class="section-title">Dry run</h3>
			<Button
				variant="secondary"
				size="sm"
				onclick={handleDryRun}
				loading={dryRunLoading}
				disabled={!config}
				data-testid="netbox-dry-run-button"
				ariaLabel={dryRunLoading ? 'Computing…' : 'Run dry-run'}
			>
				{dryRunLoading ? 'Computing…' : 'Run dry-run'}
			</Button>
		</div>
		{#if !config}
			<p class="hint" data-testid="netbox-dry-run-needs-config">
				Save a NetBox config above before running a dry-run.
			</p>
		{:else if dryRunError}
			<NetboxErrorBanner
				heading="Dry-run failed."
				message={netboxDryRunErrorText(dryRunError)}
				testId="netbox-dry-run-error-banner"
				code={dryRunError.code}
			/>
		{:else if dryRunPlan}
			<NetboxDryRunTable plan={dryRunPlan} />
		{:else}
			<p class="hint" data-testid="netbox-dry-run-empty">
				Compute the plan to preview exactly what an export would create, update, or skip —
				no writes are made.
			</p>
		{/if}
	</section>

	<section class="section" aria-labelledby="netbox-runs-heading">
		<h3 id="netbox-runs-heading" class="section-title">Run history</h3>
		{#if runsError}
			<NetboxErrorBanner
				heading="Could not load run history."
				message={runsError}
				testId="netbox-runs-error-banner"
			/>
		{/if}
		{#if retryError}
			<NetboxErrorBanner
				heading="Retry failed."
				message={netboxRetryErrorText(retryError)}
				testId="netbox-retry-error"
				code={retryError.code}
			/>
		{/if}
		<NetboxRunHistory
			{runs}
			loading={runsLoading}
			{currentRun}
			{runLoading}
			{retrying}
			onSelectRun={handleSelectRun}
			onRetry={handleRetry}
		/>
	</section>
</section>

<style>
	.panel {
		display: flex;
		flex-direction: column;
		gap: 1rem;
		padding: 1rem;
		background: var(--bg-surface);
		border: 1px solid var(--color-neutral-200);
		border-radius: var(--radius-sm);
	}
	.panel-header {
		display: flex;
		justify-content: space-between;
		align-items: flex-start;
		gap: 0.75rem;
		flex-wrap: wrap;
	}
	.header-left { display: flex; flex-direction: column; gap: 0.25rem; }
	.header-right { display: flex; align-items: center; gap: 0.5rem; }
	.panel-title {
		font-size: var(--text-sm);
		font-weight: 700;
		margin: 0;
		color: var(--color-neutral-700);
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}
	.panel-sub { font-size: 12px; color: var(--color-neutral-500); }
	.section {
		display: flex;
		flex-direction: column;
		gap: 0.6rem;
		padding-top: 0.75rem;
		border-top: 1px solid var(--color-neutral-200);
	}
	.section-header {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 0.5rem;
		flex-wrap: wrap;
	}
	.section-title {
		margin: 0;
		font-size: var(--text-sm);
		font-weight: 700;
		color: var(--color-neutral-700);
	}
	.hint { margin: 0; font-size: 12px; color: var(--color-neutral-500); }
</style>
