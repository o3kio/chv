<script lang="ts">
	/**
	 * Read-only banner shown above the metadata card on starter architecture
	 * detail pages. Conveys to operators that the topology is system-owned
	 * and the supported workflow is "clone, then edit" — see
	 * `docs/plans/2026-06-16-starter-topologies-and-auto-seed.md` §5.
	 */
</script>

<div class="banner" role="note" data-testid="starter-banner">
	<strong>System-provided starter topology.</strong>
	<span>This architecture is owned by the system. Clone it to make your own editable copy.</span>
</div>

<style>
	.banner {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		padding: 0.75rem 1rem;
		background: rgba(59, 130, 246, 0.08);
		border: 1px solid rgba(59, 130, 246, 0.4);
		border-radius: var(--radius-xs);
		color: rgb(30, 64, 175);
		font-size: var(--chv-text-sm);
	}
</style>
