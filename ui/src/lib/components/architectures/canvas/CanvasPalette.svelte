<script lang="ts">
	/**
	 * Drag-source palette rail for the Architecture Designer canvas.
	 *
	 * Renders one button per `PaletteEntry`. Each button advertises its kind
	 * via the HTML5 drag data type `application/chv-palette-kind`; the canvas
	 * pane reads that value on drop and converts it into a new node via
	 * `architectureCanvasStore.addNode(kind, position, name)`.
	 *
	 * Lucide icons are picked at render time from a static dictionary so
	 * tree-shaking still works (no dynamic `lucide-svelte/icons/<name>`
	 * imports). Adding a new palette kind requires adding a row here AND in
	 * `palette.ts` — the agent A `palette.test.ts` enforces parity at compile
	 * time.
	 */
	import { PALETTE } from '#lib/components/architectures/canvas/palette.ts';
	import type { NodeKind } from '#lib/components/architectures/canvas/edge-rules.ts';
	import type { ComponentType, SvelteComponent } from 'svelte';
	import { Server, Network, HardDrive, Disc, Layers, Box, User, Shield } from 'lucide-svelte';

	// `lucide-svelte` v1.0.x ships icons as legacy `SvelteComponentTyped`
	// classes; widen the dictionary type to `ComponentType` so TypeScript
	// accepts the assignment without losing the "must be a Svelte component"
	// guarantee.
	// eslint-disable-next-line @typescript-eslint/no-explicit-any
	type IconComponent = ComponentType<SvelteComponent<any, any, any>>;

	const ICON_FOR_KIND: Record<NodeKind, IconComponent> = {
		host: Server,
		network: Network,
		datastore: HardDrive,
		image: Disc,
		template: Layers,
		instance: Box,
		user: User,
		role: Shield
	};

	export const PALETTE_DRAG_TYPE = 'application/chv-palette-kind';

	function onDragStart(event: DragEvent, kind: NodeKind): void {
		if (!event.dataTransfer) return;
		event.dataTransfer.setData(PALETTE_DRAG_TYPE, kind);
		event.dataTransfer.effectAllowed = 'copy';
	}
</script>

<aside class="palette" aria-label="Resource palette" data-testid="canvas-palette">
	<h3 class="palette-title">Add resource</h3>
	<ul class="palette-list">
		{#each PALETTE as entry (entry.kind)}
			{@const Icon = ICON_FOR_KIND[entry.kind]}
			<li>
				<button
					type="button"
					class="palette-item"
					draggable="true"
					ondragstart={(e) => onDragStart(e, entry.kind)}
					title={entry.description}
					data-testid={`palette-tile-${entry.kind}`}
					data-palette-kind={entry.kind}
				>
					<span class="icon" aria-hidden="true"><Icon size={18} /></span>
					<span class="label">{entry.label}</span>
				</button>
			</li>
		{/each}
	</ul>
	<p class="hint">Drag a resource onto the canvas to add it.</p>
</aside>

<style>
	.palette {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		padding: 0.75rem;
		width: 180px;
		background: var(--chv-color-neutral-50, #f7f3ec);
		border-right: 1px solid var(--chv-color-neutral-200, #ddd5c8);
		font-family: var(--chv-font-sans, 'IBM Plex Sans', system-ui, sans-serif);
	}
	.palette-title {
		margin: 0;
		font-size: var(--chv-text-xs, 0.6875rem);
		font-weight: 700;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--chv-color-neutral-700, #423b33);
	}
	.palette-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.3rem;
	}
	.palette-item {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		width: 100%;
		padding: 0.45rem 0.55rem;
		text-align: left;
		font-size: var(--chv-text-sm, 0.8125rem);
		font-weight: 500;
		color: var(--chv-color-neutral-800, #29241f);
		background: var(--bg-surface, #fff);
		border: 1px solid var(--chv-color-neutral-300, #c7bcac);
		border-radius: var(--radius-sm, 0.25rem);
		cursor: grab;
	}
	.palette-item:hover {
		background: var(--color-primary-light, #f5eadc);
		border-color: var(--color-primary, #8f5a2a);
	}
	.palette-item:focus-visible {
		outline: 2px solid var(--color-primary, #8f5a2a);
		outline-offset: 1px;
	}
	.palette-item:active {
		cursor: grabbing;
	}
	.icon {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 24px;
		height: 24px;
		color: var(--color-primary-dark, #5e3513);
	}
	.hint {
		margin: 0.25rem 0 0 0;
		font-size: var(--chv-text-xs, 0.6875rem);
		color: var(--chv-color-neutral-500, #75695b);
		line-height: 1.3;
	}
</style>
