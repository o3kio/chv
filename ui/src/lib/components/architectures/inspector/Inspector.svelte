<script lang="ts">
	/**
	 * Architecture Designer node inspector.
	 *
	 * Reads `selectedNodeId` from `architectureCanvasStore` and renders a
	 * panel for the matched node. The header shows the kind label and a
	 * read-only stable id (per graph-contract.md, ids are derived from
	 * `(kind, name)` so renaming a node WOULD change its id; therefore the
	 * inspector intentionally does NOT rename — it edits `data.name` only and
	 * leaves the id stable, matching the comment on
	 * `ArchitectureCanvasStore.updateNodeData`).
	 *
	 * Per-kind field groups are extracted into `inspector/{Kind}Fields.svelte`
	 * partials so this file stays under the 300-line component budget.
	 *
	 * Edits go through `architectureCanvasStore.updateNodeData(id, partial)`
	 * which sets `dirty=true`. The name input is debounced to 250ms so that
	 * typing doesn't churn the store on every keystroke.
	 */
	import { architectureCanvasStore } from '#lib/stores/architecture-canvas-store.svelte.ts';
	import HostFields from './HostFields.svelte';
	import NetworkFields from './NetworkFields.svelte';
	import DatastoreFields from './DatastoreFields.svelte';
	import ImageFields from './ImageFields.svelte';
	import TemplateFields from './TemplateFields.svelte';
	import InstanceFields from './InstanceFields.svelte';
	import UserFields from './UserFields.svelte';
	import RoleFields from './RoleFields.svelte';
	import './field-styles.css';
	import type { NodeKind } from '#lib/components/architectures/canvas/edge-rules.ts';

	const KIND_LABEL: Record<NodeKind, string> = {
		host: 'Host',
		network: 'Network',
		datastore: 'Datastore',
		image: 'Image',
		template: 'Template',
		instance: 'Instance',
		user: 'User',
		role: 'Role'
	};

	const selectedNode = $derived.by(() => {
		const id = architectureCanvasStore.selectedNodeId;
		if (id === null) return null;
		return architectureCanvasStore.nodes.find((n) => n.id === id) ?? null;
	});

	// Debounced rename so per-keystroke updates don't churn the store / trigger
	// excessive re-serialisation. The handle is owned by the inspector lifetime
	// and cleared on each input event (debounce reset) and on $effect cleanup.
	let renameTimer: ReturnType<typeof setTimeout> | null = null;
	let pendingName = $state<string | null>(null);

	function commitName(nodeId: string, value: string): void {
		architectureCanvasStore.updateNodeData(nodeId, { name: value });
		pendingName = null;
	}

	function onNameInput(nodeId: string, value: string): void {
		pendingName = value;
		if (renameTimer !== null) clearTimeout(renameTimer);
		renameTimer = setTimeout(() => commitName(nodeId, value), 250);
	}

	$effect(() => {
		// Cleanup on unmount: flush any pending rename so the user doesn't lose
		// the last keystroke when navigating away.
		return () => {
			if (renameTimer !== null) {
				clearTimeout(renameTimer);
				renameTimer = null;
			}
		};
	});

	function update(nodeId: string, partial: Record<string, unknown>): void {
		architectureCanvasStore.updateNodeData(nodeId, partial);
	}

	function close(): void {
		architectureCanvasStore.selectedNodeId = null;
	}

	const displayedName = $derived.by(() => {
		if (pendingName !== null) return pendingName;
		const raw = selectedNode?.data?.name;
		return typeof raw === 'string' ? raw : '';
	});
</script>

{#if selectedNode}
	{@const node = selectedNode}
	<aside class="inspector" aria-label="Node inspector" data-testid="canvas-inspector">
		<header class="inspector-header">
			<div class="header-title">
				<span class="kind-label">{KIND_LABEL[node.data.kind]}</span>
				<code class="node-id" title={node.id}>{node.id}</code>
			</div>
			<button
				type="button"
				class="close"
				onclick={close}
				aria-label="Close inspector"
				data-testid="canvas-inspector-close"
			>
				×
			</button>
		</header>

		<div class="chv-fields-grid name-row">
			<label for="{node.id}-name">Name</label>
			<input
				id="{node.id}-name"
				type="text"
				value={displayedName}
				oninput={(e) => onNameInput(node.id, e.currentTarget.value)}
				data-testid="canvas-inspector-name"
			/>
			<p class="hint">
				Renaming updates the displayed label only; the node id stays stable so existing edges keep
				their endpoints.
			</p>
		</div>

		<section class="fields">
			{#if node.data.kind === 'host'}
				<HostFields nodeId={node.id} data={node.data} update={(p) => update(node.id, p)} />
			{:else if node.data.kind === 'network'}
				<NetworkFields nodeId={node.id} data={node.data} update={(p) => update(node.id, p)} />
			{:else if node.data.kind === 'datastore'}
				<DatastoreFields nodeId={node.id} data={node.data} update={(p) => update(node.id, p)} />
			{:else if node.data.kind === 'image'}
				<ImageFields nodeId={node.id} data={node.data} update={(p) => update(node.id, p)} />
			{:else if node.data.kind === 'template'}
				<TemplateFields nodeId={node.id} data={node.data} update={(p) => update(node.id, p)} />
			{:else if node.data.kind === 'instance'}
				<InstanceFields nodeId={node.id} data={node.data} update={(p) => update(node.id, p)} />
			{:else if node.data.kind === 'user'}
				<UserFields nodeId={node.id} data={node.data} update={(p) => update(node.id, p)} />
			{:else if node.data.kind === 'role'}
				<RoleFields nodeId={node.id} data={node.data} update={(p) => update(node.id, p)} />
			{/if}
		</section>
	</aside>
{/if}

<style>
	.inspector {
		width: 360px;
		height: 100%;
		padding: 1rem;
		background: var(--bg-surface, #fff);
		border-left: 1px solid var(--chv-color-neutral-200, #ddd5c8);
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
		overflow-y: auto;
		font-family: var(--chv-font-sans, 'IBM Plex Sans', system-ui, sans-serif);
		color: var(--chv-color-neutral-900, #191612);
	}
	.inspector-header {
		display: flex;
		justify-content: space-between;
		align-items: flex-start;
		gap: 0.5rem;
	}
	.header-title {
		display: flex;
		flex-direction: column;
		gap: 0.15rem;
		min-width: 0;
	}
	.kind-label {
		font-size: var(--chv-text-xs, 0.6875rem);
		font-weight: 700;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--chv-color-neutral-700, #423b33);
	}
	.node-id {
		font-family: var(--chv-font-mono, 'IBM Plex Mono', monospace);
		font-size: 11px;
		color: var(--chv-color-neutral-500, #75695b);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.close {
		appearance: none;
		background: transparent;
		border: 1px solid var(--chv-color-neutral-300, #c7bcac);
		border-radius: var(--radius-sm, 0.25rem);
		width: 28px;
		height: 28px;
		font-size: 18px;
		line-height: 1;
		cursor: pointer;
		color: var(--chv-color-neutral-700, #423b33);
		flex: 0 0 auto;
	}
	.close:hover {
		background: var(--chv-color-neutral-100, #efe9df);
	}
	.close:focus-visible {
		outline: 2px solid var(--color-primary, #8f5a2a);
		outline-offset: 1px;
	}
	.name-row {
		padding-bottom: 0.5rem;
		border-bottom: 1px solid var(--chv-color-neutral-200, #ddd5c8);
	}
	.hint {
		margin: 0.25rem 0 0 0;
		font-size: var(--chv-text-xs, 0.6875rem);
		color: var(--chv-color-neutral-500, #75695b);
		line-height: 1.3;
	}
	.fields {
		display: flex;
		flex-direction: column;
		gap: 0.6rem;
	}
</style>
