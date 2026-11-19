<script lang="ts">
	import { goto } from '$app/navigation';
	import Button from '#lib/components/primitives/Button.svelte';
	import RunStatusBadge from '#lib/components/architectures/runs/RunStatusBadge.svelte';
	import type { PageData } from './$types';

	let { data }: { data: PageData } = $props();
	const model = $derived(data.model);

	const heading = $derived(
		model.state === 'ready'
			? `Runs · ${model.architecture.display_name ?? model.architecture.name}`
			: 'Runs'
	);

	function fmtDuration(start: string | null, end: string | null): string {
		if (!start) return '—';
		const s = new Date(start).getTime();
		const e = end ? new Date(end).getTime() : Date.now();
		const ms = Math.max(0, e - s);
		const sec = Math.round(ms / 1000);
		if (sec < 60) return `${sec}s`;
		const m = Math.floor(sec / 60);
		const r = sec % 60;
		return `${m}m ${r}s`;
	}

	function fmtTime(iso: string | null): string {
		if (!iso) return '—';
		try {
			return new Date(iso).toLocaleString();
		} catch {
			return iso;
		}
	}
</script>

<svelte:head>
	<title>{heading} · CellHV</title>
</svelte:head>

<div class="page" data-testid="architecture-runs-page">
	{#if model.state === 'error'}
		<div class="error-banner" role="alert" data-testid="runs-error">
			<strong>Could not load runs for {model.id}.</strong>
			<span>{model.errorMessage}</span>
			<div>
				<Button variant="secondary" size="sm" onclick={() => goto('/architectures')}>
					Back to architectures
				</Button>
			</div>
		</div>
	{:else}
		<header class="page-header">
			<div>
				<button
					type="button"
					class="back-link"
					onclick={() => goto(`/architectures/${model.architecture.id}`)}
					aria-label="Back to architecture detail"
				>
					← {model.architecture.display_name ?? model.architecture.name}
				</button>
				<h1 class="page-title">Apply runs</h1>
				<p class="page-subtitle">History of apply and destroy runs for this architecture.</p>
			</div>
		</header>

		{#if model.runs.length === 0}
			<div class="empty" role="status" data-testid="runs-empty">
				<p class="et">No runs yet.</p>
				<p class="ex">
					Generate a plan from the dashboard and apply it to start a run.
				</p>
			</div>
		{:else}
			<div class="table-wrap" data-testid="runs-list">
				<table class="runs">
					<caption class="sr-only">Apply runs for {model.architecture.name}</caption>
					<thead>
						<tr>
							<th scope="col">Started</th>
							<th scope="col">Status</th>
							<th scope="col">Plan</th>
							<th scope="col">Requested by</th>
							<th scope="col">Duration</th>
							<th scope="col"><span class="sr-only">Actions</span></th>
						</tr>
					</thead>
					<tbody>
						{#each model.runs as run (run.id)}
							<tr data-testid="runs-row" data-run-id={run.id}>
								<td data-testid="runs-row-started-at">{fmtTime(run.started_at ?? run.created_at)}</td>
								<td><RunStatusBadge status={run.status} /></td>
								<td data-testid="runs-row-plan">
									{run.plan_id ? run.plan_id : '—'}
								</td>
								<td data-testid="runs-row-requested-by">{run.requested_by ?? '—'}</td>
								<td data-testid="runs-row-duration">
									{fmtDuration(run.started_at, run.finished_at)}
								</td>
								<td>
									<a
										class="row-link"
										href={`/architectures/${model.architecture.id}/runs/${run.id}`}
										data-testid="runs-row-link"
										aria-label={`Open run ${run.id}`}
									>
										View →
									</a>
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
	{/if}
</div>

<style>
	.page { display: flex; flex-direction: column; gap: 1rem; }
	.page-header { display: flex; flex-direction: column; gap: 0.25rem; }
	.back-link {
		background: none;
		border: none;
		padding: 0;
		font-size: var(--chv-text-xs);
		color: var(--chv-color-neutral-500);
		cursor: pointer;
		align-self: flex-start;
	}
	.back-link:hover, .back-link:focus-visible {
		color: var(--color-primary);
		outline: none;
	}
	.page-title { font-size: var(--chv-text-lg); font-weight: 700; margin: 0; }
	.page-subtitle { margin: 0; font-size: var(--chv-text-sm); color: var(--chv-color-neutral-600); }
	.empty {
		padding: 2rem;
		border: 1px dashed var(--chv-color-neutral-300);
		border-radius: var(--radius-sm);
		background: var(--chv-color-neutral-50, #f8fafc);
		text-align: center;
	}
	.et { margin: 0; font-size: var(--chv-text-sm); font-weight: 600; color: var(--chv-color-neutral-700); }
	.ex { margin: 0.25rem 0 0 0; font-size: 12px; color: var(--chv-color-neutral-500); }
	.table-wrap {
		border: 1px solid var(--chv-color-neutral-200);
		border-radius: var(--radius-sm);
		overflow-x: auto;
	}
	.runs { width: 100%; border-collapse: collapse; font-size: var(--chv-text-sm); }
	.runs th, .runs td {
		padding: 0.5rem 0.75rem;
		text-align: left;
		border-bottom: 1px solid var(--chv-color-neutral-200);
	}
	.runs thead th {
		background: var(--chv-color-neutral-50, #f8fafc);
		font-size: 12px;
		font-weight: 700;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--chv-color-neutral-600);
	}
	.runs tbody tr:last-child td { border-bottom: none; }
	.row-link {
		color: var(--color-primary);
		text-decoration: none;
		font-size: 12px;
	}
	.row-link:hover, .row-link:focus-visible { text-decoration: underline; }
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
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		padding: 0;
		margin: -1px;
		overflow: hidden;
		clip: rect(0, 0, 0, 0);
		white-space: nowrap;
		border: 0;
	}
</style>
