<script lang="ts">
	import Modal from '../primitives/Modal.svelte';
	import { AlertTriangle } from 'lucide-svelte';
	import Button from '#lib/components/primitives/Button.svelte';

	interface Props {
		open?: boolean;
		instanceName: string;
		instanceId: string;
		onConfirm: () => void;
		onCancel: () => void;
	}

	let {
		open = $bindable(false),
		instanceName,
		instanceId,
		onConfirm,
		onCancel
	}: Props = $props();

	let confirmText = $state('');
	let isDeleting = $state(false);

	const trimmedConfirm = $derived(confirmText.trim());
	const canDelete = $derived(trimmedConfirm === instanceName);

	async function handleConfirm() {
		if (!canDelete) return;
		isDeleting = true;
		try {
			onConfirm();
		} finally {
			isDeleting = false;
			confirmText = '';
		}
	}

	function handleCancel() {
		confirmText = '';
		isDeleting = false;
		onCancel();
	}

	$effect(() => {
		if (!open) {
			confirmText = '';
			isDeleting = false;
		}
	});
</script>

<Modal bind:open closeOnBackdrop={true} onClose={handleCancel}>
	{#snippet header()}
		<div class="flex items-center gap-3">
			<AlertTriangle class="h-5 w-5 text-danger" aria-hidden="true" />
			<h2 id="modal-title" class="text-base font-semibold text-(--shell-text)">
				Delete instance "{instanceName}"?
			</h2>
		</div>
	{/snippet}

	<div class="space-y-4">
		<p class="text-sm text-(--shell-text-secondary)">
			This permanently removes the instance configuration and selected related resources. This action
			cannot be undone.
		</p>

		<div class="bg-(--shell-surface-muted) rounded-lg p-3 border border-(--shell-line)">
			<p class="text-xs font-medium text-(--shell-text-muted) uppercase tracking-wider mb-2">
				This will delete
			</p>
			<ul class="space-y-1 text-sm text-(--shell-text)">
				<li class="flex items-center gap-2">
					<span class="w-1 h-1 rounded-full bg-danger" aria-hidden="true"></span>
					Instance configuration
				</li>
				<li class="flex items-center gap-2">
					<span class="w-1 h-1 rounded-full bg-danger" aria-hidden="true"></span>
					Root disk
				</li>
				<li class="flex items-center gap-2">
					<span class="w-1 h-1 rounded-full bg-danger" aria-hidden="true"></span>
					Cloud-init disk
				</li>
				<li class="flex items-center gap-2">
					<span class="w-1 h-1 rounded-full bg-danger" aria-hidden="true"></span>
					Runtime state
				</li>
			</ul>
		</div>

		<div class="space-y-2 pt-2 border-t border-(--shell-line)">
			<p class="text-sm text-(--shell-text-secondary)">
				To confirm, type <code class="bg-(--shell-surface-muted) px-1.5 py-0.5 rounded-sm text-xs font-mono border border-(--shell-line)">{instanceName}</code>
				below:
			</p>
			<input
				type="text"
				bind:value={confirmText}
				placeholder={`Type "${instanceName}" to confirm`}
				class="w-full px-3 py-2 text-sm border border-(--shell-line) rounded-sm bg-(--shell-surface) text-(--shell-text) placeholder:text-(--shell-text-muted) focus:outline-hidden focus:ring-2 focus:ring-primary/30 focus:border-primary"
				aria-label="Type instance name to confirm deletion"
			/>
		</div>
	</div>

	{#snippet footer()}
		<Button variant="secondary" size="sm" onclick={handleCancel}>Cancel</Button>
		<Button
			variant="danger"
			size="sm"
			disabled={!canDelete || isDeleting}
			loading={isDeleting}
			onclick={handleConfirm}
			ariaLabel="Permanently delete instance {instanceName}"
		>
			{isDeleting ? 'Deleting…' : 'Delete Instance'}
		</Button>
	{/snippet}
</Modal>
