<script lang="ts">
	import { AlertTriangle, RefreshCw } from 'lucide-svelte';
	import Button from '#lib/components/primitives/Button.svelte';

	let { onReload }: { onReload: () => void } = $props();
</script>

<div class="banner" role="alert" data-testid="stale-version-banner">
	<div class="banner-icon" aria-hidden="true">
		<AlertTriangle size={18} />
	</div>
	<div class="banner-body">
		<div class="banner-title">This architecture changed while you were editing</div>
		<p class="banner-description">
			Someone else saved a newer version. Reload to see their changes before editing again.
		</p>
	</div>
	<div class="banner-action">
		<Button variant="secondary" onclick={onReload} ariaLabel="Reload architecture">
			<RefreshCw size={14} aria-hidden="true" />
			Reload
		</Button>
	</div>
</div>

<style>
	.banner {
		display: flex;
		gap: 0.75rem;
		align-items: flex-start;
		padding: 0.75rem 1rem;
		background: rgba(245, 158, 11, 0.1);
		border: 1px solid rgba(245, 158, 11, 0.4);
		border-radius: var(--radius-xs);
		color: rgb(120, 75, 0);
	}

	.banner-icon {
		color: rgb(180, 113, 0);
		padding-top: 2px;
	}

	.banner-body {
		flex: 1;
		min-width: 0;
	}

	.banner-title {
		font-weight: 700;
		font-size: var(--chv-text-sm);
		margin-bottom: 0.15rem;
		color: rgb(120, 75, 0);
	}

	.banner-description {
		margin: 0;
		font-size: 12px;
		color: rgb(120, 75, 0);
	}

	.banner-action {
		flex-shrink: 0;
	}
</style>
