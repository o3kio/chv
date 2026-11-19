<script lang="ts">
	import { LayoutGrid } from 'lucide-svelte';
	import Button from '#lib/components/primitives/Button.svelte';
	import { goto } from '$app/navigation';

	let { ctaHref = '/architectures/new' }: { ctaHref?: string } = $props();
</script>

<div
	class="empty-state"
	role="status"
	aria-live="polite"
	data-testid="architectures-empty-state"
>
	<div class="empty-state-icon" aria-hidden="true">
		<LayoutGrid size={48} />
	</div>
	<h2 class="empty-state-title">No architectures yet</h2>
	<p class="empty-state-description">
		Design your first topology by sketching hosts, networks, datastores and
		instances. Architectures stay as drafts until you validate, plan and apply
		them.
	</p>
	<div class="empty-state-actions">
		<Button variant="primary" onclick={() => goto(ctaHref)}>
			Create your first
		</Button>
	</div>
</div>

<style>
	.empty-state {
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		padding: 3rem 1.5rem;
		text-align: center;
	}

	.empty-state-icon {
		color: var(--chv-color-neutral-300);
		margin-bottom: 1rem;
	}

	.empty-state-title {
		font-size: var(--chv-text-lg);
		font-weight: 600;
		color: var(--chv-color-neutral-700);
		margin: 0 0 0.5rem 0;
	}

	.empty-state-description {
		font-size: var(--chv-text-sm);
		color: var(--chv-color-neutral-500);
		max-width: 480px;
		margin: 0 0 1.5rem 0;
		line-height: 1.5;
	}

	.empty-state-actions {
		display: flex;
		gap: 0.75rem;
		flex-wrap: wrap;
		justify-content: center;
	}
</style>
