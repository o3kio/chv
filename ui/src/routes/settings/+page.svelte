<script lang="ts">
import PageHeaderWithAction from '#lib/components/shell/PageHeaderWithAction.svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import PropertyGrid from '#lib/components/shell/PropertyGrid.svelte';
	import StatusBadge from '#lib/components/shell/StatusBadge.svelte';
	import CompactMetricCard from '#lib/components/shared/CompactMetricCard.svelte';
	import ErrorState from '#lib/components/shell/ErrorState.svelte';
	import { 
		Settings, Shield, Users as UsersIcon,
		Terminal, User, Cpu
	} from 'lucide-svelte';
	import { getPageDefinition } from '#lib/shell/app-shell.ts';
	import type { PageData } from './$types';

	let { data }: { data: PageData } = $props();

	const page = getPageDefinition('/settings');
	const model = $derived(data.settings);
	const operatorCount = $derived(
		model.usersState === 'error' ? '—' : String(model.users.length)
	);

	const envProps = $derived([
		{ label: 'Control Plane', value: model.version },
		{ label: 'Build Hash', value: model.build, isMono: true },
		{ label: 'Environment', value: model.environment },
		{ label: 'API Endpoint', value: model.api_endpoint, isMono: true }
	]);

	const accessProps = $derived([
		{ label: 'Session TTL', value: `${model.session_ttl_hours} hours` },
		{ label: 'Active Operators', value: operatorCount }
	]);
</script>

<div class="inventory-page">
	{#if model.state === 'error'}
		<ErrorState title="Governance registry unreachable" description="Failed to retrieve configuration from the central control plane." />
	{:else}
		<PageHeaderWithAction page={page} />

		<div class="inventory-metrics">
			<CompactMetricCard 
				label="Control Plane" 
				value={model.version} 
				color="primary"
			/>
			<CompactMetricCard 
				label="Environment" 
				value={model.environment} 
				color="neutral"
			/>
			<CompactMetricCard 
				label="Operator Count" 
				value={operatorCount} 
				color="neutral"
			/>
			<CompactMetricCard 
				label="Session TTL" 
				value={`${model.session_ttl_hours}h`} 
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
						{#if model.usersState === 'error'}
							<p class="roster-empty">Operator roster unavailable — the control plane did not return it.</p>
						{:else if model.users.length === 0}
							<p class="roster-empty">No operator principals enrolled.</p>
						{:else}
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
						{/if}
					</div>
				</SectionCard>
			</div>

			<aside class="support-area">
				<SectionCard title="Administrative Ops" icon={Settings}>
					<div class="ops-pipeline">
						<a href="/settings/hypervisor" class="btn-secondary op-button">
							<Cpu size={14} />
							Fabric Parameters
						</a>
						<a href="/settings/users" class="btn-secondary op-button">
							<UsersIcon size={14} />
							Authorize Principal
						</a>
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

	.roster-empty {
		font-size: 11px;
		color: var(--chv-color-neutral-500);
		margin: 0;
	}

	.ops-pipeline {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}

	.op-button {
		display: flex;
		align-items: center;
		gap: 0.35rem;
		width: 100%;
		justify-content: flex-start;
		font-size: 10px;
		font-weight: 700;
		text-transform: uppercase;
	}

	@media (max-width: 1100px) {
		.inventory-main {
			grid-template-columns: 1fr;
		}
	}

</style>
