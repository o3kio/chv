<script lang="ts">
	import { fade } from 'svelte/transition';

	interface Props {
		resource: {
			type: string;
			name: string;
			status: string;
			tone: string;
			meta: string;
			actions: { label: string; href: string }[];
		};
		getStatusColor: (status: string) => string;
	}

	let { resource, getStatusColor }: Props = $props();
</script>

<aside class="selection-panel" aria-label="Selected topology component" transition:fade={{ duration: 120 }}>
	<div>
		<p class="selection-panel__eyebrow">{resource.type}</p>
		<h3>{resource.name}</h3>
		<p>{resource.meta}</p>
	</div>
	<div class="selection-panel__status">
		<span style:background={getStatusColor(resource.tone)}></span>
		<strong>{resource.status}</strong>
	</div>
	<div class="selection-panel__actions">
		{#each resource.actions as action}
			<a href={action.href}>{action.label}</a>
		{/each}
	</div>
</aside>

<style>
	.selection-panel {
		position: absolute;
		left: 0.75rem;
		bottom: 0.75rem;
		display: grid;
		gap: 0.55rem;
		width: min(18rem, calc(100% - 1.5rem));
		padding: 0.75rem;
		border: 1px solid var(--shell-line-strong);
		border-radius: var(--radius-sm);
		background: color-mix(in srgb, var(--shell-surface) 94%, transparent);
		box-shadow: var(--chv-shadow-sm);
	}

	.selection-panel__eyebrow,
	.selection-panel p,
	.selection-panel h3 {
		margin: 0;
	}

	.selection-panel__eyebrow {
		font-size: 9px;
		font-weight: 700;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--shell-text-muted);
	}

	.selection-panel h3 {
		margin-top: 0.15rem;
		font-size: var(--chv-text-sm);
		color: var(--shell-text);
	}

	.selection-panel p {
		margin-top: 0.2rem;
		font-size: var(--chv-text-xs);
		color: var(--shell-text-muted);
	}

	.selection-panel__status {
		display: flex;
		align-items: center;
		gap: 0.35rem;
		font-size: var(--chv-text-xs);
		color: var(--shell-text);
	}

	.selection-panel__status span {
		width: 0.45rem;
		height: 0.45rem;
		border-radius: 999px;
		background: var(--color-success);
	}

	.selection-panel__actions {
		display: flex;
		flex-wrap: wrap;
		gap: 0.35rem;
	}

	.selection-panel__actions a {
		padding: 0.25rem 0.45rem;
		border: 1px solid var(--shell-line);
		border-radius: var(--radius-xs);
		color: var(--shell-text);
		font-size: var(--chv-text-xs);
		text-decoration: none;
		background: var(--shell-surface-muted);
	}

	.selection-panel__actions a:hover {
		border-color: var(--shell-line-strong);
		color: var(--shell-accent);
	}

	@media (prefers-reduced-motion: reduce) {
		.selection-panel {
			transition: none;
		}
	}
</style>
