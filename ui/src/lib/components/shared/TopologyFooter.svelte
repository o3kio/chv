<div class="canvas-footer">
	<div class="legend">
		<div class="legend-item"><div class="dot dc"></div> Datacenter</div>
		<div class="legend-item"><div class="dot node"></div> Node</div>
		<div class="legend-item"><div class="dot vm"></div> Workload</div>
	</div>
</div>

<style>
	.canvas-footer {
		padding: 0.5rem 0.75rem;
		border-top: 1px solid var(--border-subtle);
		background: var(--bg-surface-muted);
	}

	.legend {
		display: flex;
		gap: 1rem;
	}

	.legend-item {
		display: flex;
		align-items: center;
		gap: 0.35rem;
		font-size: 10px;
		color: var(--chv-color-neutral-500);
	}

	.dot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
	}

	.dot.dc { background: var(--color-info); }
	.dot.node { background: var(--color-primary); }
	.dot.vm { background: var(--color-accent); }
</style>
