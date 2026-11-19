<script lang="ts">
	import { goto } from '$app/navigation';
	import Button from '#lib/components/primitives/Button.svelte';
	import ResourceDetailHeader from '#lib/components/shell/ResourceDetailHeader.svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import PropertyGrid from '#lib/components/shell/PropertyGrid.svelte';
	import {
		AlertTriangle,
		ChevronRight,
		Info,
		ShieldCheck,
		Activity,
		ArrowLeft,
		RotateCcw
	} from 'lucide-svelte';

	interface Props {
		vmId?: string;
		requestedVmId?: string;
		currentTab?: string;
		nodeId?: string;
		errorMessage?: string;
		onRetry?: () => void;
	}

	let {
		vmId,
		requestedVmId,
		currentTab,
		nodeId,
		errorMessage,
		onRetry
	}: Props = $props();
</script>

<ResourceDetailHeader
	title="Requested workload unreachable"
	eyebrow={`VM ID ${requestedVmId ?? vmId}`}
	statusLabel="unreachable"
	tone="failed"
	description="The control plane could not assemble a reliable VM record for this route. Keep the operator in a recovery workflow instead of a blank dead end."
	parentLabel="Virtual machines"
	parentHref="/vms"
>
	{#snippet actions()}
		<div class="header-actions">
			<Button variant="secondary" onclick={() => goto('/vms')}>
				<ArrowLeft size={14} />
				Back to Catalog
			</Button>
			<Button variant="primary" onclick={onRetry}>
				<RotateCcw size={14} />
				Retry Lookup
			</Button>
		</div>
	{/snippet}
</ResourceDetailHeader>

<div class="detail-recovery">
	<section class="detail-recovery__lead">
		<div class="recovery-hero">
			<div class="recovery-hero__icon">
				<AlertTriangle size={18} />
			</div>
			<div class="recovery-hero__copy">
				<h2>Workload telemetry could not be resolved.</h2>
				<p>
					The requested VM may still exist, but the control plane could not join live guest
					signals, placement data, or recent task state into a usable record.
				</p>
				<span>Most often this is a transient API gap, a stale route, or a node-side reporting interruption.</span>
			</div>
		</div>

		<SectionCard title="Recovery Paths" icon={ChevronRight} badgeLabel="Operator Actions">
			<div class="recovery-actions-grid">
				<a href="/vms" class="recovery-action">
					<strong>Return to virtual machines</strong>
					<span>Check whether the workload is still listed and whether its posture changed.</span>
				</a>
				<a href="/" class="recovery-action">
					<strong>Open fleet overview</strong>
					<span>Look for node degradation, alert spikes, or task backlog before retrying.</span>
				</a>
				<a href="/events" class="recovery-action">
					<strong>Inspect event stream</strong>
					<span>Use the incident feed to confirm whether this is a routing problem or a guest-side failure.</span>
				</a>
			</div>
		</SectionCard>
	</section>

	<aside class="detail-recovery__rail">
		<SectionCard title="Requested Object" icon={Info}>
			<PropertyGrid
				columns={1}
				properties={[
					{ label: 'Requested VM ID', value: requestedVmId ?? (vmId || 'Unknown') },
					{ label: 'Requested tab', value: currentTab || 'summary' },
					{ label: 'Known host', value: nodeId || 'Not available' }
				]}
			/>
		</SectionCard>

		<SectionCard title="Operator Checklist" icon={ShieldCheck}>
			<ul class="recovery-checklist">
				<li>Confirm the workload still exists in the VM catalog.</li>
				<li>Verify the host node is still reporting into the control plane.</li>
				<li>Retry the lookup after the fleet event queue settles.</li>
			</ul>
		</SectionCard>

		<SectionCard title="Failure Shape" icon={Activity}>
			<div class="recovery-facts">
				{#if errorMessage}
					<p><strong>Error:</strong> {errorMessage}</p>
				{:else}
					<p>No guest summary was returned for this route.</p>
				{/if}
				<span>The page stayed in recovery mode rather than showing stale runtime controls.</span>
			</div>
		</SectionCard>
	</aside>
</div>

<style>
	.header-actions {
		display: flex;
		flex-wrap: wrap;
		gap: 0.25rem;
		align-items: center;
		justify-content: flex-end;
	}

	.detail-recovery {
		display: grid;
		grid-template-columns: minmax(0, 1.55fr) minmax(18rem, 0.85fr);
		gap: 1rem;
		align-items: start;
	}

	.detail-recovery__lead,
	.detail-recovery__rail {
		display: flex;
		flex-direction: column;
		gap: 1rem;
		min-width: 0;
	}

	.recovery-hero {
		display: grid;
		grid-template-columns: auto 1fr;
		gap: 1rem;
		padding: 1rem 1.1rem;
		border-radius: var(--radius-md);
		border: 1px solid var(--color-danger);
		background: linear-gradient(180deg, var(--color-danger-light), color-mix(in srgb, var(--color-danger-light) 35%, white));
	}

	.recovery-hero__icon {
		display: grid;
		place-items: center;
		width: 2.8rem;
		height: 2.8rem;
		border-radius: var(--radius-sm);
		border: 1px solid var(--color-danger);
		color: var(--color-danger-dark);
		background: var(--bg-surface);
	}

	.recovery-hero__copy h2,
	.recovery-hero__copy p,
	.recovery-hero__copy span {
		margin: 0;
	}

	.recovery-hero__copy h2 {
		font-size: var(--chv-text-lg);
		line-height: 1.2;
		color: var(--shell-text);
	}

	.recovery-hero__copy p {
		margin-top: 0.4rem;
		font-size: var(--chv-text-sm);
		line-height: 1.55;
		color: var(--shell-text);
		max-width: 46rem;
	}

	.recovery-hero__copy span {
		display: block;
		margin-top: 0.55rem;
		font-size: var(--chv-text-xs);
		line-height: 1.5;
		color: var(--shell-text-muted);
	}

	.recovery-actions-grid {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 0.75rem;
	}

	.recovery-action {
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
		padding: 0.85rem 0.9rem;
		border-radius: var(--radius-sm);
		border: 1px solid var(--shell-line);
		background: var(--shell-surface-muted);
		text-decoration: none;
		color: inherit;
	}

	.recovery-action strong {
		font-size: var(--chv-text-sm);
		color: var(--shell-text);
	}

	.recovery-action span {
		font-size: var(--chv-text-xs);
		line-height: 1.5;
		color: var(--shell-text-muted);
	}

	.recovery-action:hover {
		border-color: var(--shell-accent);
		background: color-mix(in srgb, var(--shell-surface-muted) 70%, var(--color-primary-light));
	}

	.recovery-checklist {
		display: flex;
		flex-direction: column;
		gap: 0.55rem;
		padding-left: 1rem;
		margin: 0;
		font-size: var(--chv-text-sm);
		line-height: 1.5;
		color: var(--shell-text);
	}

	.recovery-facts {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
	}

	.recovery-facts p,
	.recovery-facts span {
		margin: 0;
	}

	.recovery-facts p {
		font-size: var(--chv-text-sm);
		color: var(--shell-text);
	}

	.recovery-facts span {
		font-size: var(--chv-text-xs);
		line-height: 1.5;
		color: var(--shell-text-muted);
	}

	@media (max-width: 1200px) {
		.detail-recovery {
			grid-template-columns: 1fr;
		}

		.recovery-actions-grid {
			grid-template-columns: 1fr;
		}
	}
</style>
