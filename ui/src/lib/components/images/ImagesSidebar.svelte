<script lang="ts">
	import { Download } from 'lucide-svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';

	interface PendingImage {
		name: string;
		size: string;
	}

	interface Props {
		pendingImages: PendingImage[];
	}

	let { pendingImages }: Props = $props();
</script>

<aside class="support-area">
	<SectionCard title="Ingestion Pipeline" icon={Download} badgeLabel={String(pendingImages.length)}>
		{#if pendingImages.length === 0}
			<p class="empty-hint">No active artifact transmissions detected.</p>
		{:else}
			<ul class="attention-list">
				{#each pendingImages as img}
					<li>
						<div class="attention-card">
							<div class="attention-card__main">
								<span class="res-name">{img.name}</span>
								<span class="res-issue">Ingesting · {img.size}</span>
							</div>
						</div>
					</li>
				{/each}
			</ul>
		{/if}
	</SectionCard>
</aside>

<style>
	.support-area {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.empty-hint {
		font-size: 11px;
		color: var(--chv-color-neutral-400);
		padding: 1rem;
		text-align: center;
	}

	.attention-list {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
		list-style: none;
		padding: 0;
		margin: 0;
	}

	.attention-card {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 0.5rem 0.75rem;
		background: var(--bg-surface-muted);
		border-radius: var(--radius-xs);
		color: var(--chv-color-neutral-800);
	}

	.attention-card__main {
		display: flex;
		flex-direction: column;
	}

	.res-name {
		font-size: 11px;
		font-weight: 700;
	}

	.res-issue {
		font-size: 9px;
		color: var(--color-warning);
		font-weight: 600;
		text-transform: uppercase;
	}
</style>
