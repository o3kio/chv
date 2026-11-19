<script lang="ts">
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import { BarChart3 } from 'lucide-svelte';

	interface Props {
		cpu: string;
		memory: string;
		powerState: string;
		health: string;
		attachedVolumes?: Record<string, unknown>[];
		attachedNics?: Record<string, unknown>[];
	}

	let {
		cpu,
		memory,
		powerState,
		health,
		attachedVolumes = [],
		attachedNics = []
	}: Props = $props();
</script>

<SectionCard title="VM Metrics" icon={BarChart3}>
	<div class="metrics-grid">
		<div class="metric-card">
			<span class="metric-label">CPU Assigned</span>
			<span class="metric-value">{cpu || '—'}</span>
		</div>
		<div class="metric-card">
			<span class="metric-label">Memory Assigned</span>
			<span class="metric-value">{memory || '—'}</span>
		</div>
		<div class="metric-card">
			<span class="metric-label">Power State</span>
			<span class="metric-value">{powerState || '—'}</span>
		</div>
		<div class="metric-card">
			<span class="metric-label">Health</span>
			<span class="metric-value">{health || '—'}</span>
		</div>
		<div class="metric-card">
			<span class="metric-label">Attached Volumes</span>
			<span class="metric-value">{attachedVolumes.length}</span>
		</div>
		<div class="metric-card">
			<span class="metric-label">Attached NICs</span>
			<span class="metric-value">{attachedNics.length}</span>
		</div>
	</div>
</SectionCard>

<style>
	.metrics-grid {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
		gap: 0.75rem;
	}

	.metric-card {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		padding: 1rem;
		background: var(--chv-color-neutral-50, #f9fafb);
		border: 1px solid var(--shell-line);
		border-radius: var(--radius-md);
	}

	.metric-label {
		font-size: var(--chv-text-xs);
		color: var(--shell-text-muted);
		font-weight: 600;
	}

	.metric-value {
		font-size: var(--chv-text-lg, 1.125rem);
		font-weight: 800;
		color: var(--shell-text);
	}
</style>
