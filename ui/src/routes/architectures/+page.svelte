<script lang="ts">
	import { goto } from '$app/navigation';
	import Button from '#lib/components/primitives/Button.svelte';
	import ArchitectureCard from '#lib/components/architectures/dashboard/ArchitectureCard.svelte';
	import EmptyState from '#lib/components/architectures/dashboard/EmptyState.svelte';
	import { getArchitectureDrift, type DriftStatus } from '#lib/bff/architectures.ts';
	import { getStoredToken } from '#lib/api/client.ts';
	import type { PageData } from './$types';

	let { data }: { data: PageData } = $props();
	const model = $derived(data.architectures);

	// Phase 6 carve-out: the dashboard fans out one drift fetch per topology
	// on mount. Acceptable at MVP scale; Phase 7 will add a denormalized
	// `architecture_topologies.last_drift_status` column so the list endpoint
	// returns the badge data inline (single round-trip).
	let driftByArch = $state<Record<string, DriftStatus>>({});

	// Fan-out coordination state. We memoize on a stable id-set hash so the
	// effect does not re-fire when sibling state (e.g. a row's
	// `display_name`) changes without changing the id list. `currentGen`
	// guards against stale-closure races: if a newer effect kicks off
	// before the previous one's fetches resolve, the old result is dropped.
	let currentGen = 0;
	let lastIdsKey = '';
	const idsKey = $derived(
		model.state === 'ready' ? model.items.map((a) => a.id).sort().join(',') : ''
	);

	$effect(() => {
		if (!idsKey || idsKey === lastIdsKey) return;
		lastIdsKey = idsKey;
		const myGen = ++currentGen;
		const ids = idsKey.split(',');
		const controller = new AbortController();
		// Read the token once per fan-out: every per-id call gets the same
		// snapshot. Without this, a Viewer/Operator/Admin user would hit
		// the operator-tier `/v1/architectures/drift` endpoint with no
		// `Authorization` header and the BFF's 401 handler in `bffFetch`
		// would force a logout — see fix-up commit referenced in Phase 8
		// release notes.
		const token = getStoredToken() ?? undefined;
		void Promise.all(
			ids.map((id) =>
				getArchitectureDrift(id, false, token, controller.signal).catch(
					() => null
				)
			)
		).then((results) => {
			if (myGen !== currentGen) return; // newer effect ran; drop stale.
			const next: Record<string, DriftStatus> = {};
			ids.forEach((id, i) => {
				const r = results[i];
				if (r) next[id] = r.status;
			});
			driftByArch = next;
		});

		return () => {
			controller.abort();
		};
	});
</script>

<svelte:head>
	<title>Architectures · CellHV</title>
</svelte:head>

<div class="page" data-testid="architectures-page">
	<header class="page-header">
		<div>
			<h1 class="page-title">Saved Topologies</h1>
			<p class="page-subtitle">Architecture drafts and applied topologies for this cloud.</p>
		</div>
		<Button variant="primary" onclick={() => goto('/architectures/new')}>
			New architecture
		</Button>
	</header>

	{#if model.state === 'error'}
		<div class="error-banner" role="alert" data-testid="architectures-error">
			<strong>Could not load architectures.</strong>
			<span>{model.errorMessage}</span>
		</div>
	{:else if model.state === 'empty'}
		<EmptyState />
	{:else}
		<div class="grid" role="list" data-testid="architectures-list">
			{#each model.items as architecture (architecture.id)}
				<div role="listitem">
					<ArchitectureCard
						{architecture}
						driftStatus={driftByArch[architecture.id]}
					/>
				</div>
			{/each}
		</div>
	{/if}
</div>

<style>
	.page {
		display: flex;
		flex-direction: column;
		gap: 1.25rem;
	}

	.page-header {
		display: flex;
		justify-content: space-between;
		align-items: flex-start;
		gap: 1rem;
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

	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(280px, 1fr));
		gap: 0.75rem;
	}

	.error-banner {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		padding: 0.75rem 1rem;
		background: rgba(220, 38, 38, 0.08);
		border: 1px solid rgba(220, 38, 38, 0.4);
		border-radius: var(--radius-xs);
		color: rgb(153, 27, 27);
		font-size: var(--chv-text-sm);
	}
</style>
