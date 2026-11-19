<script lang="ts">
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import PropertyGrid from '#lib/components/shell/PropertyGrid.svelte';
	import { ChevronRight, Info, ShieldCheck, ChevronLeft } from 'lucide-svelte';

	interface ConfigProp {
		label: string;
		value: string;
	}

	interface Props {
		nodeId: string;
		configProps: ConfigProp[];
		open: boolean;
		onToggle: () => void;
	}

	let {
		nodeId,
		configProps,
		open,
		onToggle
	}: Props = $props();
</script>

<aside class="support-area" class:support-area--collapsed={!open}>
	<div class="support-rail-control">
		<button
			class="support-toggle"
			type="button"
			onclick={onToggle}
			title={open ? 'Minimize details' : 'Expand details'}
			aria-label={open ? 'Minimize details' : 'Expand details'}
		>
			{#if open}
				<ChevronRight size={14} />
			{:else}
				<ChevronLeft size={14} />
			{/if}
		</button>

		{#if !open}
			<button class="support-rail-tab" type="button" onclick={onToggle} title="Placement audit" aria-label="Expand placement audit">
				<ChevronRight size={13} />
			</button>
			<button class="support-rail-tab" type="button" onclick={onToggle} title="Workload meta" aria-label="Expand workload meta">
				<Info size={13} />
			</button>
			<button class="support-rail-tab" type="button" onclick={onToggle} title="Safety integrity" aria-label="Expand safety integrity">
				<ShieldCheck size={13} />
			</button>
		{/if}
	</div>

	{#if open}
		<SectionCard title="Placement Audit" icon={ChevronRight}>
			<PropertyGrid
				columns={1}
				properties={[
					{ label: 'Fabric Host', value: nodeId },
					{ label: 'Security Domain', value: 'BALANCED' },
					{ label: 'Hypervisor Sub', value: 'CLOUD_HYPERVISOR_v3' }
				]}
			/>
		</SectionCard>

		<SectionCard title="Workload Meta" icon={Info}>
			<PropertyGrid properties={configProps} columns={1} />
		</SectionCard>

		<SectionCard title="Safety Integrity" icon={ShieldCheck}>
			<div class="safety-sign">
				<ShieldCheck size={16} />
				<span>GUEST_LEVEL_NOMINAL</span>
			</div>
		</SectionCard>
	{/if}
</aside>

<style>
	.support-area {
		display: flex;
		flex-direction: column;
		gap: 1rem;
		min-width: 0;
		position: sticky;
		top: 0.75rem;
	}

	.support-area--collapsed {
		align-items: stretch;
		gap: 0.35rem;
	}

	.support-rail-control {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
	}

	.support-toggle,
	.support-rail-tab {
		display: grid;
		place-items: center;
		width: 2.2rem;
		height: 2.2rem;
		border: 1px solid var(--shell-line);
		border-radius: var(--radius-xs);
		background: var(--shell-surface);
		color: var(--shell-text-muted);
		cursor: pointer;
	}

	.support-toggle:hover,
	.support-rail-tab:hover {
		border-color: var(--shell-accent);
		background: var(--shell-accent-soft);
		color: var(--shell-text);
	}

	.safety-sign {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		padding: 0.65rem 0.75rem;
		border-radius: var(--radius-sm);
		background: var(--color-success-light);
		color: var(--color-success-dark);
		font-size: var(--chv-text-sm);
		font-weight: 600;
	}

	@media (max-width: 1200px) {
		.support-area {
			position: static;
			align-items: flex-start;
		}

		.support-rail-control {
			flex-direction: row;
		}
	}
</style>
