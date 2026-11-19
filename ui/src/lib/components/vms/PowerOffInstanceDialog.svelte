<script lang="ts">
	import Modal from '../primitives/Modal.svelte';
	import { AlertCircle } from 'lucide-svelte';
	import Button from '#lib/components/primitives/Button.svelte';

	interface Props {
		open?: boolean;
		instanceName: string;
		onConfirm: () => void;
		onCancel: () => void;
	}

	let {
		open = $bindable(false),
		instanceName,
		onConfirm,
		onCancel
	}: Props = $props();

	let isProcessing = $state(false);

	async function handleConfirm() {
		isProcessing = true;
		try {
			onConfirm();
		} finally {
			isProcessing = false;
		}
	}

	function handleCancel() {
		isProcessing = false;
		onCancel();
	}

	$effect(() => {
		if (!open) {
			isProcessing = false;
		}
	});
</script>

<Modal bind:open closeOnBackdrop={true} onClose={handleCancel}>
	{#snippet header()}
		<div class="flex items-center gap-3">
			<AlertCircle class="h-5 w-5 text-warning" aria-hidden="true" />
			<h2 id="modal-title" class="text-base font-semibold text-(--shell-text)">
				Power off instance "{instanceName}"?
			</h2>
		</div>
	{/snippet}

	<div class="space-y-4">
		<div class="flex items-start gap-3 p-3 bg-(--color-warning-light) border border-warning/20 rounded-lg">
			<AlertCircle class="h-5 w-5 text-(--color-warning-dark) shrink-0 mt-0.5" aria-hidden="true" />
			<div>
				<p class="text-sm font-medium text-(--shell-text)">Immediate hard stop</p>
				<p class="text-sm text-(--shell-text-secondary) mt-1">
					This does not gracefully shut down the guest operating system and may cause data loss.
					Use Shutdown for a graceful stop.
				</p>
			</div>
		</div>

		<p class="text-sm text-(--shell-text-secondary)">
			Instance <strong>{instanceName}</strong> will be forcefully stopped.
		</p>
	</div>

	{#snippet footer()}
		<Button variant="secondary" size="sm" onclick={handleCancel}>Cancel</Button>
		<Button
			variant="danger"
			size="sm"
			loading={isProcessing}
			disabled={isProcessing}
			onclick={handleConfirm}
			ariaLabel="Power off instance {instanceName}"
		>
			{isProcessing ? 'Powering off…' : 'Power Off'}
		</Button>
	{/snippet}
</Modal>
