<script lang="ts">
	import Modal from '#lib/components/primitives/Modal.svelte';
	import Button from '#lib/components/primitives/Button.svelte';

	/**
	 * Confirmation dialog for `apply` and `destroy` actions.
	 *
	 * Required-confirmation rules
	 *   - typed-name: required when `planMode === 'destroy'` OR
	 *     `hasDestructiveChanges === true`. Disabled state on the submit
	 *     button until `typedName.trim() === architectureName`.
	 *   - acknowledge-warnings: required when `warnings.length > 0`.
	 *
	 * The component is dumb: it does not call the BFF directly; the parent
	 * passes `onConfirm` and decides what action to take. This keeps the
	 * dialog reusable across apply and destroy paths.
	 *
	 * Accessibility
	 *   - Modal primitive already wires role="dialog", aria-modal,
	 *     aria-labelledby, focus trap, and Esc-to-close. We add explicit
	 *     describedby copy for the warnings list and typed-name help text.
	 */

	interface Props {
		open: boolean;
		architectureName: string;
		planMode: 'apply' | 'destroy';
		warnings: string[];
		hasDestructiveChanges: boolean;
		/** Submit-button loading state (parent owns the request). */
		submitting?: boolean;
		onConfirm: (typedName: string, acknowledgedWarnings: boolean) => void;
		onCancel: () => void;
	}

	let {
		open = $bindable(false),
		architectureName,
		planMode,
		warnings,
		hasDestructiveChanges,
		submitting = false,
		onConfirm,
		onCancel
	}: Props = $props();

	let typedName = $state('');
	let ackWarnings = $state(false);

	const requireTypedName = $derived(planMode === 'destroy' || hasDestructiveChanges);
	const typedNameMatches = $derived(typedName.trim() === architectureName);

	const requireAck = $derived(warnings.length > 0);

	const canSubmit = $derived(
		!submitting &&
			(!requireTypedName || typedNameMatches) &&
			(!requireAck || ackWarnings)
	);

	const title = $derived(planMode === 'destroy' ? 'Confirm destroy' : 'Confirm apply');
	const submitLabel = $derived(
		submitting
			? planMode === 'destroy'
				? 'Submitting destroy…'
				: 'Submitting apply…'
			: planMode === 'destroy'
				? 'Apply destroy plan'
				: 'Apply plan'
	);

	function reset() {
		typedName = '';
		ackWarnings = false;
	}

	function handleCancel() {
		reset();
		onCancel();
	}

	function handleConfirm() {
		if (!canSubmit) return;
		const submittedTypedName = requireTypedName ? typedName.trim() : '';
		const submittedAck = ackWarnings;
		// Reset only on confirm-success path is owned by the parent (it
		// closes the dialog after navigation). Keeping local state across
		// errors lets the user retry without re-typing.
		onConfirm(submittedTypedName, submittedAck);
	}

	// Reset when the dialog re-opens so prior input doesn't leak between
	// distinct invocations.
	$effect(() => {
		if (open) {
			typedName = '';
			ackWarnings = false;
		}
	});
</script>

<Modal bind:open {title} width="default" onClose={handleCancel}>
	<div class="body" data-testid="apply-confirm-dialog">
		<p class="lead">
			You are about to {planMode === 'destroy' ? 'destroy' : 'apply changes to'}
			<strong>{architectureName}</strong>.
		</p>

		{#if warnings.length > 0}
			<section class="warns" aria-labelledby="apply-warns-heading">
				<h3 id="apply-warns-heading" class="wht">Warnings</h3>
				<ul class="wl" data-testid="apply-confirm-warnings">
					{#each warnings as w, i (i)}
						<li data-testid="apply-confirm-warning">{w}</li>
					{/each}
				</ul>
				<label class="ack">
					<input
						type="checkbox"
						bind:checked={ackWarnings}
						data-testid="apply-acknowledge-warnings"
						aria-describedby="apply-warns-heading"
					/>
					<span>I understand and want to proceed despite these warnings.</span>
				</label>
			</section>
		{/if}

		{#if requireTypedName}
			<section class="confirm">
				<label for="apply-typed-name-input" class="cl">
					Type the architecture name <strong>{architectureName}</strong> to confirm.
				</label>
				<input
					id="apply-typed-name-input"
					type="text"
					class="ci"
					bind:value={typedName}
					autocomplete="off"
					spellcheck="false"
					data-testid="apply-typed-name-input"
					aria-describedby="apply-typed-name-help"
					aria-invalid={requireTypedName && typedName.length > 0 && !typedNameMatches}
				/>
				<p id="apply-typed-name-help" class="ch">
					{#if planMode === 'destroy'}
						Destroy plans always require typed-name confirmation.
					{:else}
						This plan contains destructive changes; typed-name confirmation is required.
					{/if}
				</p>
			</section>
		{/if}
	</div>

	{#snippet footer()}
		<Button
			variant="ghost"
			size="sm"
			onclick={handleCancel}
			disabled={submitting}
			data-testid="apply-cancel-button"
			ariaLabel="Cancel"
		>Cancel</Button>
		<Button
			variant={planMode === 'destroy' ? 'danger' : 'primary'}
			size="sm"
			onclick={handleConfirm}
			disabled={!canSubmit}
			loading={submitting}
			data-testid="apply-confirm-button"
			ariaLabel={planMode === 'destroy' ? 'Confirm destroy' : 'Confirm apply'}
		>{submitLabel}</Button>
	{/snippet}
</Modal>

<style>
	.body { display: flex; flex-direction: column; gap: 0.85rem; }
	.lead { margin: 0; font-size: var(--chv-text-sm); color: var(--chv-color-neutral-700); }
	.warns {
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
		padding: 0.6rem 0.85rem;
		background: rgba(245, 158, 11, 0.08);
		border: 1px solid rgba(245, 158, 11, 0.4);
		border-radius: var(--radius-xs);
	}
	.wht {
		margin: 0;
		font-size: 12px;
		font-weight: 700;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--chv-color-neutral-700);
	}
	.wl {
		margin: 0;
		padding-left: 1.1rem;
		font-size: 12px;
		color: var(--chv-color-neutral-700);
	}
	.ack {
		display: flex;
		gap: 0.4rem;
		font-size: var(--chv-text-sm);
		color: var(--chv-color-neutral-700);
		cursor: pointer;
	}
	.confirm { display: flex; flex-direction: column; gap: 0.3rem; }
	.cl { font-size: var(--chv-text-sm); color: var(--chv-color-neutral-700); }
	.ci {
		padding: 0.4rem 0.6rem;
		font-size: var(--chv-text-sm);
		border: 1px solid var(--chv-color-neutral-300);
		border-radius: var(--radius-xs);
		background: var(--bg-surface);
		color: var(--chv-color-neutral-900);
	}
	.ci:focus-visible { outline: 2px solid var(--color-primary); outline-offset: 1px; }
	.ci[aria-invalid='true'] { border-color: var(--color-danger, #b91c1c); }
	.ch { margin: 0; font-size: 12px; color: var(--chv-color-neutral-500); }
</style>
