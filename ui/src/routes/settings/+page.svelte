<script lang="ts">
import Button from '#lib/components/primitives/Button.svelte';
	import PageHeaderWithAction from '#lib/components/shell/PageHeaderWithAction.svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import PropertyGrid from '#lib/components/shell/PropertyGrid.svelte';
	import StatusBadge from '#lib/components/shell/StatusBadge.svelte';
	import CompactMetricCard from '#lib/components/shared/CompactMetricCard.svelte';
	import ErrorState from '#lib/components/shell/ErrorState.svelte';
	import { 
		Settings, Shield, Fingerprint, Users as UsersIcon, 
		History, Lock, Terminal, User, Cpu, Activity
	} from 'lucide-svelte';
	import { getPageDefinition } from '#lib/shell/app-shell.ts';
	import type { PageData } from './$types';

	let { data }: { data: PageData } = $props();

	const page = getPageDefinition('/settings');
	const model = $derived(data.settings);

	const envProps = $derived([
		{ label: 'Control Plane', value: model.version },
		{ label: 'Build Hash', value: model.build, isMono: true },
		{ label: 'Environment', value: model.environment },
		{ label: 'API Endpoint', value: model.api_endpoint, isMono: true }
	]);

	const accessProps = $derived([
		{ label: 'Session TTL', value: `${model.session_ttl_hours} hours` },
		{ label: 'Active Operators', value: String(model.users.length) }
	]);
</script>

<div class="inventory-page">
	{#if model.state === 'error'}
		<ErrorState title="Governance registry unreachable" description="Failed to retrieve configuration from the central control plane." />
	{:else}
		<PageHeaderWithAction page={page} />

		<div class="inventory-metrics">
			<CompactMetricCard 
				label="Platform Integrity" 
				value="VERIFIED" 
				color="primary"
			/>
			<CompactMetricCard 
				label="Operator Count" 
				value={model.users.length} 
				color="neutral"
			/>
			<CompactMetricCard 
				label="Governance Mode" 
				value="STRICT" 
				color="primary"
			/>
			<CompactMetricCard 
				label="Identity Epoch" 
				value="v1.4.2" 
				color="neutral"
			/>
		</div>

		<main class="inventory-main">
			<div class="settings-content">
				<SectionCard title="Infrastructure Environment" icon={Terminal}>
					<PropertyGrid properties={envProps} columns={2} />
				</SectionCard>

				<SectionCard title="Access & Identity Registry" icon={Shield}>
					<PropertyGrid properties={accessProps} columns={2} />
					
					<div class="operators-roster">
						<div class="roster-header">Authorized Principal Identities</div>
						<ul class="operator-list">
							{#each model.users as user}
								<li>
									<div class="operator-card">
										<div class="operator-info">
											<User size={12} />
											<span class="operator-id">{user.email}</span>
										</div>
										<StatusBadge label={user.role} tone="healthy" />
									</div>
								</li>
							{/each}
						</ul>
					</div>
				</SectionCard>

				<SectionCard title="Security Hardening" icon={Lock}>
					<div class="governance-alert">
						<Fingerprint size={16} />
						<div class="alert-content">
							<span class="alert-title">Advanced Policies Engaged</span>
							<p class="alert-desc">Identity rotation, audit-locking, and cryptographic sealing are active across the fleet.</p>
						</div>
					</div>
				</SectionCard>
			</div>

			<aside class="support-area">
				<SectionCard title="Administrative Ops" icon={Settings}>
					<div class="ops-pipeline">
						<Button variant="secondary" class="op-button">
							<History size={14} />
							Audit Registry
						</Button>
						<Button variant="secondary" class="op-button">
							<Lock size={14} />
							Seal Control Plane
						</Button>
						<a href="/settings/hypervisor" class="btn-secondary op-button">
							<Cpu size={14} />
							Fabric Parameters
						</a>
						<Button variant="primary" class="op-button">
							<UsersIcon size={14} />
							Authorize Principal
						</Button>
					</div>
				</SectionCard>

				<SectionCard title="Compliance State" icon={Activity}>
					<div class="compliance-summary">
						<div class="summary-row">
							<span>ISO-27001</span>
							<span>VERIFIED</span>
						</div>
						<div class="summary-row">
							<span>SOC2-ATT</span>
							<span>READY</span>
						</div>
					</div>
				</SectionCard>
			</aside>
		</main>
	{/if}
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

	.inventory-main {
		display: grid;
		grid-template-columns: 1fr 300px;
		gap: 1rem;
		align-items: start;
	}

	.settings-content {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.support-area {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.operators-roster {
		margin-top: 1.25rem;
		padding-top: 1rem;
		border-top: 1px solid var(--border-subtle);
	}

	.roster-header {
		font-size: 10px;
		font-weight: 800;
		text-transform: uppercase;
		color: var(--chv-color-neutral-500);
		letter-spacing: 0.05em;
		margin-bottom: 0.75rem;
	}

	.operator-list {
		list-style: none;
		padding: 0;
		margin: 0;
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
	}

	.operator-card {
		display: flex;
		justify-content: space-between;
		align-items: center;
		padding: 0.5rem 0.75rem;
		background: var(--bg-surface-muted);
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-xs);
	}

	.operator-info {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		font-size: 11px;
		font-weight: 700;
		color: var(--chv-color-neutral-900);
	}

	.governance-alert {
		display: flex;
		gap: 1rem;
		padding: 0.75rem;
		background: var(--bg-surface-muted);
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-xs);
	}

	.alert-title {
		display: block;
		font-size: 11px;
		font-weight: 800;
		color: var(--chv-color-neutral-900);
		margin-bottom: 0.125rem;
	}

	.alert-desc {
		font-size: 10px;
		color: var(--chv-color-neutral-500);
		margin: 0;
	}

	.ops-pipeline {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}

	.op-button {
		width: 100%;
		justify-content: flex-start;
		font-size: 10px;
		font-weight: 700;
		text-transform: uppercase;
	}

	.compliance-summary {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
	}

	.summary-row {
		display: flex;
		justify-content: space-between;
		font-size: 10px;
		color: var(--chv-color-neutral-600);
		padding: 0.35rem 0.5rem;
		background: var(--bg-surface-muted);
		border-radius: var(--radius-xs);
	}

	.summary-row span:last-child {
		font-weight: 800;
		color: var(--color-primary);
	}

	@media (max-width: 1100px) {
		.inventory-main {
			grid-template-columns: 1fr;
		}
	}

</style>
