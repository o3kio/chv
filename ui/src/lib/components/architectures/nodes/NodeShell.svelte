<script lang="ts">
	/**
	 * Shared visual shell for every Architecture Designer node component.
	 *
	 * Per the Phase-2 spec the eight per-kind node components must differ only
	 * in icon + label. The card chrome (size, border, selected state, validation
	 * badge, source/target handles) is identical, so we centralise it here.
	 *
	 * Each per-kind file (`HostNode.svelte` et al.) is a thin wrapper that:
	 *   1. picks an icon from `lucide-svelte`,
	 *   2. picks a kind label,
	 *   3. delegates the rest to this shell.
	 *
	 * The shell is internal: it does not appear in the `nodeTypes` map — Svelte
	 * Flow only sees the eight per-kind wrappers — so SvelteFlow's
	 * `NodeProps`-typed surface is preserved at the wrapper boundary.
	 */
	import { Handle, Position, type NodeProps } from '@xyflow/svelte';
	import type { ComponentType, SvelteComponent } from 'svelte';

	type FindingSeverity = 'error' | 'warning' | 'info' | 'clean';

	/**
	 * Icon component type. `lucide-svelte` v1.0.x ships icons as legacy class
	 * components (`SvelteComponentTyped`) rather than Svelte 5 `Component<>`
	 * function components, so we widen the type with `ComponentType<SvelteComponent>`
	 * here. Not `any` — TypeScript still verifies the prop is a Svelte
	 * component, just not a specific signature.
	 */
	// eslint-disable-next-line @typescript-eslint/no-explicit-any
	type IconComponent = ComponentType<SvelteComponent<any, any, any>>;

	interface Props extends NodeProps {
		/** Icon component from lucide-svelte. */
		Icon: IconComponent;
		/** Singular kind label rendered as the dim subtitle ("Host", "Network"). */
		kindLabel: string;
		/**
		 * Machine-readable kind slug (`host`, `instance`, …). Used for stable
		 * `data-testid="canvas-node-<kind>"` selectors that the Playwright
		 * spec keys off — agent C's `architectures-canvas.spec.ts` expects
		 * exactly this naming.
		 */
		kindId: string;
		/**
		 * Severity badge to render. Resolved upstream by `Canvas.svelte` from
		 * the validation findings list — node components don't query findings
		 * directly so they remain pure.
		 *
		 * `'clean'` renders a neutral pill; `'info'` is rendered the same as
		 * clean because finding badges in CHV's design system only differentiate
		 * error/warning/clean (info findings stay in the side panel).
		 */
		findingSeverity?: FindingSeverity;
	}

	let { id, data, selected, Icon, kindLabel, kindId, findingSeverity = 'clean' }: Props = $props();

	// Pull `name` out defensively — `data` is typed as `Record<string, unknown>`
	// at the SvelteFlow boundary (xyflow types intentionally widen `data`), so
	// we never assume the shape beyond what `CanvasNodeData` guarantees.
	const displayName = $derived.by(() => {
		const raw = (data as { name?: unknown }).name;
		return typeof raw === 'string' && raw.length > 0 ? raw : 'untitled';
	});

	// Map severity to design-system tokens. `info` collapses to `clean` per
	// the comment above; this keeps the badge legend small (red/yellow/gray).
	const badgeLabel: Record<FindingSeverity, string> = {
		error: 'Error',
		warning: 'Warning',
		info: 'OK',
		clean: 'OK'
	};
	const showBadgePill = $derived(findingSeverity === 'error' || findingSeverity === 'warning');
</script>

<div class="chv-node" class:selected data-testid={`canvas-node-${kindId}`} data-finding-severity={findingSeverity}>
	<Handle type="target" position={Position.Top} data-testid={`node-handle-target-${id}`} />
	<div class="row">
		<span class="icon" aria-hidden="true">
			<Icon size={20} />
		</span>
		<div class="text">
			<span class="name" title={displayName}>{displayName}</span>
			<span class="kind">{kindLabel}</span>
		</div>
		{#if showBadgePill}
			<span
				class="badge badge-{findingSeverity}"
				aria-label={`${badgeLabel[findingSeverity]} finding on this node`}
				data-testid="canvas-node-badge"
			>
				{badgeLabel[findingSeverity]}
			</span>
		{/if}
	</div>
	<Handle type="source" position={Position.Bottom} data-testid={`node-handle-source-${id}`} />
</div>

<style>
	.chv-node {
		width: 180px;
		min-height: 80px;
		padding: 0.625rem 0.75rem;
		border-radius: var(--radius-md, 0.5rem);
		background: var(--chv-color-neutral-50, #f7f3ec);
		border: 1px solid var(--chv-color-neutral-300, #c7bcac);
		box-shadow: 0 1px 2px rgba(0, 0, 0, 0.04);
		display: flex;
		flex-direction: column;
		justify-content: center;
		font-family: var(--chv-font-sans, 'IBM Plex Sans', system-ui, sans-serif);
		color: var(--chv-color-neutral-800, #29241f);
	}

	.chv-node.selected {
		outline: 2px solid var(--color-primary, #8f5a2a);
		outline-offset: 1px;
	}

	.row {
		display: flex;
		align-items: center;
		gap: 0.5rem;
	}

	.icon {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 28px;
		height: 28px;
		border-radius: var(--radius-sm, 0.25rem);
		background: var(--color-primary-light, #f5eadc);
		color: var(--color-primary-dark, #5e3513);
		flex: 0 0 auto;
	}

	.text {
		display: flex;
		flex-direction: column;
		min-width: 0;
		flex: 1 1 auto;
	}

	.name {
		font-size: var(--chv-text-sm, 0.8125rem);
		font-weight: 600;
		line-height: 1.2;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.kind {
		font-size: var(--chv-text-xs, 0.6875rem);
		color: var(--chv-color-neutral-500, #75695b);
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}

	.badge {
		display: inline-flex;
		align-items: center;
		padding: 0.1rem 0.4rem;
		border-radius: var(--radius-full, 9999px);
		font-size: 10px;
		font-weight: 700;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		flex: 0 0 auto;
	}

	.badge-error {
		background: var(--color-danger-light, #faece8);
		color: var(--color-danger, #9b4338);
		border: 1px solid var(--color-danger, #9b4338);
	}

	.badge-warning {
		background: var(--color-warning-light, #f8efd9);
		color: var(--color-warning, #9a6a1f);
		border: 1px solid var(--color-warning, #9a6a1f);
	}
</style>
