<script lang="ts">
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import { Activity } from 'lucide-svelte';
	import type { InfrastructureEvent } from '#lib/bff/types.ts';

	interface Props {
		eventsLoading: boolean;
		eventsError: string | null;
		events: InfrastructureEvent[];
	}

	let { eventsLoading, eventsError, events }: Props = $props();
</script>

<SectionCard title="VM Events" icon={Activity}>
	{#if eventsLoading}
		<p class="empty-hint">Loading event stream...</p>
	{:else if eventsError}
		<p class="empty-hint">Event registry inaccessible: {eventsError}</p>
	{:else if events.length === 0}
		<p class="empty-hint">No events recorded for this workload.</p>
	{:else}
		<div class="events-table-wrap">
			<table class="events-table">
				<thead>
					<tr>
						<th>Timestamp</th>
						<th>Type</th>
						<th>Severity</th>
						<th>Message</th>
					</tr>
				</thead>
				<tbody>
					{#each events as event}
						<tr>
							<td class="events-ts">{new Date(event.occurred_at).toLocaleString('en-US', { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' })}</td>
							<td>{event.type}</td>
							<td><span class="severity-badge severity-badge--{event.severity}">{event.severity}</span></td>
							<td>{event.summary}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</SectionCard>

<style>
	.empty-hint {
		font-size: var(--chv-text-xs);
		color: var(--shell-text-muted);
		text-align: center;
		padding: 1rem 0;
	}

	.events-table-wrap {
		overflow-x: auto;
	}

	.events-table {
		width: 100%;
		border-collapse: collapse;
		font-size: var(--chv-text-xs);
	}

	.events-table th {
		text-align: left;
		font-weight: 700;
		color: var(--shell-text-muted);
		padding: 0.5rem 0.75rem;
		border-bottom: 1px solid var(--shell-line);
		white-space: nowrap;
	}

	.events-table td {
		padding: 0.5rem 0.75rem;
		border-bottom: 1px solid var(--shell-line);
		color: var(--shell-text);
	}

	.events-ts {
		white-space: nowrap;
		color: var(--shell-text-muted);
	}

	.severity-badge {
		display: inline-block;
		font-size: 9px;
		font-weight: 700;
		text-transform: uppercase;
		padding: 2px 6px;
		border-radius: 3px;
	}

	.severity-badge--critical {
		background: var(--color-danger-light, #fee2e2);
		color: var(--color-danger, #dc2626);
	}

	.severity-badge--warning {
		background: var(--color-warning-light, #fef3c7);
		color: var(--color-warning-dark, #92400e);
	}

	.severity-badge--info {
		background: var(--chv-color-neutral-100, #f3f4f6);
		color: var(--chv-color-neutral-600, #4b5563);
	}
</style>
