<script lang="ts">
	import type { ArchitectureSummary, DriftStatus } from '#lib/bff/architectures.ts';
	import DriftStatusBadge from '#lib/components/architectures/drift/DriftStatusBadge.svelte';

	interface Props {
		architecture: ArchitectureSummary;
		/**
		 * Per-card drift status. The dashboard fans out a `getArchitectureDrift`
		 * call per architecture on mount (Phase 6 carve-out: N round-trips, not
		 * 1 — Phase 7 will denormalize this onto the topology row). `undefined`
		 * means the fetch is still in flight or failed; we render nothing in
		 * that case to keep the dashboard quiet.
		 */
		driftStatus?: DriftStatus | undefined;
	}

	let { architecture, driftStatus }: Props = $props();

	function formatDate(iso: string | null): string {
		if (!iso) return '—';
		try {
			return new Date(iso).toLocaleString();
		} catch {
			return iso;
		}
	}

	// Card heading prefers `display_name` (the human label), falling back to
	// `name` (the slug). The slug is always present on the wire.
	const heading = $derived(architecture.display_name ?? architecture.name);
	const description = $derived(architecture.description ?? '');
	const environment = $derived(architecture.environment ?? '');

	const envClass = $derived(
		environment === 'production'
			? 'env-prod'
			: environment === 'staging'
				? 'env-staging'
				: 'env-dev'
	);
</script>

<a
	href={`/architectures/${architecture.id}`}
	class="card"
	data-testid="architecture-card"
	aria-label={`Open architecture ${heading}`}
>
	<div class="card-header">
		<div class="card-title" data-testid="architecture-card-name">
			{heading}
		</div>
		<div class="card-header-right">
			{#if driftStatus && (driftStatus === 'drifted' || driftStatus === 'check_failed' || driftStatus === 'no_drift')}
				<span data-testid="architecture-drift-badge">
					<DriftStatusBadge status={driftStatus} compact />
				</span>
			{/if}
			{#if environment}
				<span class="env-badge {envClass}" aria-label={`Environment ${environment}`}>
					{environment}
				</span>
			{/if}
		</div>
	</div>

	{#if description}
		<p class="card-description">{description}</p>
	{/if}

	<div class="card-meta">
		<span class="status-pill status-{architecture.status}">{architecture.status}</span>
		<span class="updated">v{architecture.version_number} · {formatDate(architecture.updated_at)}</span>
	</div>
</a>

<style>
	.card {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		padding: 1rem;
		background: var(--bg-surface);
		border: 1px solid var(--chv-color-neutral-200);
		border-radius: var(--radius-sm);
		text-decoration: none;
		color: inherit;
		transition: border-color 120ms ease, box-shadow 120ms ease;
	}

	.card:hover,
	.card:focus-visible {
		border-color: var(--color-primary);
		box-shadow: 0 1px 4px rgba(0, 0, 0, 0.06);
		outline: none;
	}

	.card-header {
		display: flex;
		justify-content: space-between;
		gap: 0.5rem;
		align-items: center;
	}

	.card-header-right {
		display: inline-flex;
		gap: 0.35rem;
		align-items: center;
		flex-wrap: wrap;
	}

	.card-title {
		font-weight: 600;
		font-size: var(--chv-text-sm);
		color: var(--chv-color-neutral-900);
	}

	.card-description {
		margin: 0;
		font-size: var(--chv-text-xs);
		color: var(--chv-color-neutral-600);
		line-height: 1.4;
		display: -webkit-box;
		-webkit-line-clamp: 2;
		line-clamp: 2;
		-webkit-box-orient: vertical;
		overflow: hidden;
	}

	.card-meta {
		display: flex;
		justify-content: space-between;
		align-items: center;
		font-size: 11px;
		color: var(--chv-color-neutral-500);
	}

	.env-badge {
		font-size: 10px;
		font-weight: 700;
		text-transform: uppercase;
		padding: 0.15rem 0.4rem;
		border-radius: var(--radius-xs);
		letter-spacing: 0.04em;
	}

	.env-dev {
		background: var(--chv-color-neutral-100);
		color: var(--chv-color-neutral-700);
	}

	.env-staging {
		background: rgba(245, 158, 11, 0.15);
		color: rgb(146, 92, 0);
	}

	.env-prod {
		background: rgba(220, 38, 38, 0.12);
		color: rgb(153, 27, 27);
	}

	.status-pill {
		font-size: 10px;
		font-weight: 600;
		text-transform: uppercase;
		padding: 0.1rem 0.4rem;
		border-radius: var(--radius-xs);
		background: var(--chv-color-neutral-100);
		color: var(--chv-color-neutral-700);
	}

	.status-pill.status-applied {
		background: rgba(16, 185, 129, 0.15);
		color: rgb(6, 95, 70);
	}

	.status-pill.status-failed,
	.status-pill.status-invalid,
	.status-pill.status-drifted {
		background: rgba(220, 38, 38, 0.12);
		color: rgb(153, 27, 27);
	}

	.status-pill.status-applying {
		background: rgba(59, 130, 246, 0.15);
		color: rgb(30, 64, 175);
	}
</style>
