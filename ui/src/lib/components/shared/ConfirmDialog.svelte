<script lang="ts">
	import Modal from '#lib/components/primitives/Modal.svelte';
	import { AlertTriangle } from 'lucide-svelte';

	interface Props {
		open?: boolean;
		title: string;
		description: string;
		confirmText?: string;
		cancelText?: string;
		variant?: 'danger' | 'primary';
		onConfirm: () => void;
		onCancel?: () => void;
	}

	let {
		open = $bindable(false),
		title,
		description,
		confirmText = 'Confirm',
		cancelText = 'Cancel',
		variant = 'danger',
		onConfirm,
		onCancel
	}: Props = $props();

	let confirmButtonRef = $state<HTMLButtonElement | null>(null);

	function handleConfirm() {
		open = false;
		onConfirm();
	}

	function handleCancel() {
		open = false;
		onCancel?.();
	}

	// Focus the confirm button when modal opens
	$effect(() => {
		if (open) {
			requestAnimationFrame(() => {
				confirmButtonRef?.focus();
			});
		}
	});

	const confirmButtonClasses = {
		danger: 'border border-danger text-danger hover:bg-danger/5 focus:ring-danger/30',
		primary: 'bg-primary text-white hover:bg-primary/90 focus:ring-primary/30'
	};
</script>

<Modal bind:open closeOnBackdrop={true} onClose={handleCancel}>
	{#snippet header()}
		<div class="flex items-center gap-3">
			{#if variant === 'danger'}
				<AlertTriangle class="h-5 w-5 text-warning" aria-hidden="true" />
			{/if}
			<h2 id="modal-title" class="text-base font-semibold text-ink">{title}</h2>
		</div>
	{/snippet}

	<p class="text-sm text-muted">{description}</p>

	{#snippet footer()}
		<button
			type="button"
			onclick={handleCancel}
			class="px-4 py-2 rounded-sm border border-line text-ink bg-white hover:bg-chrome transition-colors"
		>
			{cancelText}
		</button>
		<button
			bind:this={confirmButtonRef}
			type="button"
			onclick={handleConfirm}
			class="px-4 py-2 rounded-sm font-medium transition-colors focus:outline-hidden focus:ring-2 focus:ring-offset-1 {confirmButtonClasses[variant]}"
		>
			{confirmText}
		</button>
	{/snippet}
</Modal>
