<script lang="ts">
	interface TabItem {
		id: string;
		label: string;
		count?: number;
	}

	interface Props {
		tabs: TabItem[];
		currentId: string;
	}

	let { tabs, currentId }: Props = $props();
</script>

<nav class="detail-tabs" aria-label="Detail sections">
	{#each tabs as tab}
		<a
			class={`detail-tabs__link ${currentId === tab.id ? 'detail-tabs__link--active' : ''}`}
			href={`?tab=${tab.id}`}
			aria-current={currentId === tab.id ? 'page' : undefined}
		>
			<span>{tab.label}</span>
			{#if tab.count !== undefined}
				<span class="detail-tabs__count">{tab.count}</span>
			{/if}
		</a>
	{/each}
</nav>

<style>
	.detail-tabs {
		display: flex;
		flex-wrap: wrap;
		gap: 0.25rem;
		border-bottom: 1px solid var(--shell-line);
	}

	.detail-tabs__link {
		display: inline-flex;
		align-items: center;
		gap: 0.3rem;
		min-height: 1.85rem;
		padding: 0.34rem 0.52rem;
		border-radius: var(--radius-xs) var(--radius-xs) 0 0;
		border: 1px solid var(--shell-line);
		border-bottom: 0;
		background: var(--shell-surface);
		color: var(--shell-text-secondary);
		text-decoration: none;
		font-size: var(--chv-text-xs);
		font-weight: 600;
		line-height: 1;
	}

	.detail-tabs__link--active {
		border-color: var(--shell-accent);
		background: var(--shell-surface-muted);
		color: var(--shell-text);
	}

	.detail-tabs__count {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		min-width: 1rem;
		height: 1rem;
		padding: 0 0.25rem;
		border-radius: var(--radius-xs);
		background: rgba(143, 90, 42, 0.08);
		font-size: 0.64rem;
		line-height: 1;
	}
</style>
