<script lang="ts">
	import Modal from '#lib/components/primitives/Modal.svelte';
	import Button from '#lib/components/primitives/Button.svelte';

	/**
	 * Export action with the production confirmation prompt.
	 *
	 * The production gating itself is server-side (403
	 * `PRODUCTION_REQUIRES_ADMIN`, parity with apply). The UI pre-empts
	 * it: when the topology's environment is `production`, the export
	 * goes through a typed-name confirmation dialog mirroring the apply
	 * flow's `ApplyConfirmDialog` (the apply dialog gates destructive
	 * plans; here the destructive risk is writing to a production NetBox
	 * inventory). Non-production topologies export directly.
	 *
	 * The component is dumb: `onExport` is owned by the panel (backed by
	 * the store's `exportNow`).
	 */

	interface Props {
		architectureName: string;
		environment: string | null;
		exporting: boolean;
		/** Disabled when no projection config exists (the BFF would 400). */
		disabled?: boolean;
		onExport: () => Promise<void> | void;
	}

	let {
		architectureName,
		environment,
		exporting,
		disabled = false,
		onExport
	}: Props = $props();

	const isProduction = $derived(environment === 'production');

	let confirmOpen = $state(false);
	let typedName = $state('');

	const typedNameMatches = $derived(typedName.trim() === architectureName);
	const canConfirm = $derived(!exporting && typedNameMatches);

	function handleClick() {
		if (exporting || disabled) return;
		if (isProduction) {
			typedName = '';
			confirmOpen = true;
		} else {
			// Swallow rejections: the store has captured the error code
			// for the panel's inline banner and mutateWithRefresh has
			// toasted it — an unhandled rejection here would only noise
			// up the console.
			void Promise.resolve(onExport()).catch(() => {});
		}
	}

	function handleCancel() {
		if (exporting) return;
		confirmOpen = false;
	}

	async function handleConfirm() {
		if (!canConfirm) return;
		const close = () => (confirmOpen = false);
		try {
			await onExport();
			close();
		} catch {
			// NETBOX_RUN_ACTIVE / PRODUCTION_REQUIRES_ADMIN / … — the
			// store has toasted it and captured the code for the panel's
			// inline banner; keep the dialog open so the operator sees
			// the context and can retry without re-typing.
		}
	}
</script>

<Button
	variant="primary"
	size="sm"
	onclick={handleClick}
	loading={exporting}
	disabled={disabled}
	data-testid="netbox-export-button"
	ariaLabel={exporting ? 'Queuing…' : 'Export to NetBox'}
>
	{exporting ? 'Queuing…' : 'Export to NetBox'}
</Button>

<Modal
	bind:open={confirmOpen}
	title="Confirm production export"
	onClose={handleCancel}
>
	<div class="body" data-testid="netbox-export-confirm-dialog">
		<p class="lead">
			You are about to export <strong>{architectureName}</strong> to NetBox. This topology is
			tagged <strong>production</strong>, so the export writes to a production inventory —
			the request is also rejected server-side unless you are an Admin.
		</p>
		<section class="confirm">
			<label for="netbox-typed-name-input" class="cl">
				Type the architecture name <strong>{architectureName}</strong> to confirm.
			</label>
			<input
				id="netbox-typed-name-input"
				type="text"
				class="ci"
				bind:value={typedName}
				autocomplete="off"
				spellcheck="false"
				data-testid="netbox-typed-name-input"
				aria-describedby="netbox-typed-name-help"
				aria-invalid={typedName.length > 0 && !typedNameMatches}
			/>
			<p id="netbox-typed-name-help" class="ch">
				Production exports require typed-name confirmation (mirrors the apply flow's
				production gate).
			</p>
		</section>
	</div>

	{#snippet footer()}
		<Button
			variant="ghost"
			size="sm"
			onclick={handleCancel}
			disabled={exporting}
			data-testid="netbox-export-cancel-button"
			ariaLabel="Cancel"
		>
			Cancel
		</Button>
		<Button
			variant="primary"
			size="sm"
			onclick={handleConfirm}
			disabled={!canConfirm}
			loading={exporting}
			data-testid="netbox-export-confirm-button"
			ariaLabel="Confirm production export"
		>
			{exporting ? 'Queuing…' : 'Export to NetBox'}
		</Button>
	{/snippet}
</Modal>

<style>
	.body {
		display: flex;
		flex-direction: column;
		gap: 0.85rem;
	}

	.lead {
		margin: 0;
		font-size: var(--text-sm);
		color: var(--color-neutral-700);
	}

	.confirm {
		display: flex;
		flex-direction: column;
		gap: 0.3rem;
	}

	.cl {
		font-size: var(--text-sm);
		color: var(--color-neutral-700);
	}

	.ci {
		padding: 0.4rem 0.6rem;
		font-size: var(--text-sm);
		border: 1px solid var(--color-neutral-300);
		border-radius: var(--radius-xs);
		background: var(--bg-surface);
		color: var(--color-neutral-900);
	}

	.ci:focus-visible {
		outline: 2px solid var(--color-primary);
		outline-offset: 1px;
	}

	.ci[aria-invalid='true'] {
		border-color: var(--color-danger, #b91c1c);
	}

	.ch {
		margin: 0;
		font-size: 12px;
		color: var(--color-neutral-500);
	}
</style>
