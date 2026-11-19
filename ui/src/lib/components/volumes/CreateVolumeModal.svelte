<script lang="ts">
	import Modal from '#lib/components/primitives/Modal.svelte';
	import FormField from '#lib/components/shared/FormField.svelte';
	import Input from '#lib/components/primitives/TextInput.svelte';
	import Select from '#lib/components/primitives/Select.svelte';
	import { getStoredToken } from '#lib/api/client.ts';
	import { listNodes } from '#lib/bff/nodes.ts';
	import { createVolume } from '#lib/bff/volumes.ts';
	import { mutateWithRefresh } from '#lib/stores/mutation.svelte.ts';
	import {
		STORAGE_CLASSES,
		buildCreateVolumePayload,
		validateVolumeCreateInput,
		type VolumeCreateFieldErrors
	} from '#lib/webui/volume-create.ts';

	interface Props {
		open?: boolean;
		onSuccess?: () => void;
	}

	let { open = $bindable(false), onSuccess }: Props = $props();

	// Form state
	let name = $state('');
	let nodeId = $state('');
	let sizeGib = $state(10);
	let storageClass = $state('');
	let submitting = $state(false);
	let formError = $state('');
	let errors = $state<VolumeCreateFieldErrors>({});

	// Node choices, fetched like the migrate modal. No eligibility
	// filter: node existence is the server's story (fail-open — the
	// route accepts and the task surfaces a bad placement), same as
	// chvctl's `--node`.
	let nodes = $state<{ node_id: string; name: string }[]>([]);
	let loadingNodes = $state(false);

	const nodeOptions = $derived(
		nodes.map((n) => ({ value: n.node_id, label: `${n.name} (${n.node_id})` }))
	);

	const storageClassOptions = [
		{ value: '', label: 'Node default (local)' },
		...STORAGE_CLASSES.map((c) => ({ value: c, label: c }))
	];

	function resetForm() {
		name = '';
		nodeId = '';
		sizeGib = 10;
		storageClass = '';
		formError = '';
		errors = {};
	}

	async function loadNodes() {
		loadingNodes = true;
		try {
			const token = getStoredToken() ?? undefined;
			const res = await listNodes({ page: 1, page_size: 100, filters: {} }, token);
			nodes = res.items.map((n) => ({ node_id: n.node_id, name: n.name }));
		} catch (e) {
			// TODO: integrate structured logger instead of console
			// eslint-disable-next-line no-console
			console.error('Failed to load nodes', e);
		} finally {
			loadingNodes = false;
		}
	}

	function validateField(field: keyof VolumeCreateFieldErrors) {
		const fieldErrors = validateVolumeCreateInput({ name, nodeId, sizeGib, storageClass });
		errors = { ...errors, [field]: fieldErrors[field] };
	}

	function isValid(): boolean {
		return Object.keys(validateVolumeCreateInput({ name, nodeId, sizeGib, storageClass })).length === 0;
	}

	async function handleSubmit(event?: Event) {
		event?.preventDefault();

		const input = { name, nodeId, sizeGib, storageClass };
		errors = validateVolumeCreateInput(input);
		if (Object.keys(errors).length > 0) return;

		submitting = true;
		formError = '';

		try {
			// The accepted task wakes the volumes list via the existing
			// task-stream → 'volumes:' cache-pattern mapping (CreateVolume
			// is already mapped); the invalidation below covers the
			// immediate refresh.
			await mutateWithRefresh(
				() => createVolume(buildCreateVolumePayload(input), getStoredToken() ?? undefined),
				{
					patterns: ['volumes:'],
					successMessage: 'Volume creation accepted',
					errorMessage: 'Failed to create volume'
				}
			);
			open = false;
			onSuccess?.();
		} catch (err) {
			const message = err instanceof Error ? err.message : 'Failed to create volume';
			formError = message;
		} finally {
			submitting = false;
		}
	}

	$effect(() => {
		if (open) {
			loadNodes();
		} else {
			resetForm();
		}
	});
</script>

<Modal bind:open title="Allocate Block" closeOnBackdrop={!submitting}>
	<form id="create-volume-form" onsubmit={handleSubmit} class="space-y-5">
		{#if formError}
			<div class="rounded-sm border border-danger/30 bg-danger/10 px-3 py-2 text-sm text-danger" role="alert">
				{formError}
			</div>
		{/if}

		<FormField label="Name" error={errors.name} required labelFor="volume-name">
			<Input
				id="volume-name"
				bind:value={name}
				placeholder="my-data-volume"
				disabled={submitting}
				onblur={() => validateField('name')}
			/>
		</FormField>

		<FormField
			label="Node"
			error={errors.nodeId}
			required
			helper="Standalone volumes are placed on the chosen node at create time"
			labelFor="volume-node"
		>
			{#if loadingNodes}
				<div class="text-sm text-muted">Loading nodes...</div>
			{:else if nodeOptions.length === 0}
				<Input
					id="volume-node"
					bind:value={nodeId}
					placeholder="node id"
					disabled={submitting}
					onblur={() => validateField('nodeId')}
				/>
				<p class="text-xs text-muted mt-1">No nodes returned by the BFF — enter a node id manually.</p>
			{:else}
				<Select
					id="volume-node"
					bind:value={nodeId}
					options={nodeOptions}
					placeholder="Select a node..."
					error={errors.nodeId}
					disabled={submitting}
				/>
			{/if}
		</FormField>

		<FormField
			label="Size (GiB)"
			error={errors.size}
			required
			helper="Between 1 and 65536 GiB (64 TiB); provisioned at create time"
			labelFor="volume-size"
		>
			<Input
				id="volume-size"
				type="number"
				bind:value={sizeGib}
				min={1}
				max={65536}
				disabled={submitting}
				onblur={() => validateField('size')}
			/>
		</FormField>

		<FormField
			label="Storage Class"
			error={errors.storageClass}
			helper="Optional — the node default is local"
			labelFor="volume-storage-class"
		>
			<Select
				id="volume-storage-class"
				bind:value={storageClass}
				options={storageClassOptions}
				disabled={submitting}
			/>
		</FormField>
	</form>

	{#snippet footer()}
		<button
			type="button"
			onclick={() => (open = false)}
			disabled={submitting}
			class="px-4 py-2 rounded-sm border border-line text-ink bg-white hover:bg-chrome transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
		>
			Cancel
		</button>
		<button
			type="submit"
			form="create-volume-form"
			disabled={!isValid() || submitting}
			class="px-4 py-2 rounded-sm bg-primary text-white font-medium hover:bg-primary/90 transition-colors disabled:bg-primary/30 disabled:cursor-not-allowed flex items-center gap-2"
		>
			{#if submitting}
				<svg
					class="animate-spin h-4 w-4"
					xmlns="http://www.w3.org/2000/svg"
					fill="none"
					viewBox="0 0 24 24"
					aria-hidden="true"
				>
					<circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
					<path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"></path>
				</svg>
			{/if}
			{submitting ? 'Allocating...' : 'Allocate Block'}
		</button>
	{/snippet}
</Modal>
