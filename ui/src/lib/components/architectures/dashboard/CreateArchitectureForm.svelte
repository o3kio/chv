<script lang="ts">
	import { goto } from '$app/navigation';
	import Button from '#lib/components/primitives/Button.svelte';
	import { architectureStore } from '#lib/stores/architecture-store.svelte.ts';
	import {
		KNOWN_ARCHITECTURE_ENVIRONMENTS,
		type ArchitectureEnvironment
	} from '#lib/bff/architectures.ts';

	let name = $state('');
	let description = $state('');
	let environment = $state<ArchitectureEnvironment>('development');
	let submitting = $state(false);

	const NAME_PATTERN = /^[a-z0-9][a-z0-9-]{1,62}[a-z0-9]$/;
	let nameError = $derived.by(() => {
		if (!name.trim()) return null;
		if (!NAME_PATTERN.test(name.trim())) {
			return 'Use 3–64 chars: lowercase letters, digits, dashes (no leading/trailing dash).';
		}
		return null;
	});

	const canSubmit = $derived(
		name.trim().length >= 3 && nameError === null && !submitting
	);

	async function handleSubmit(event: SubmitEvent) {
		event.preventDefault();
		if (!canSubmit) return;
		submitting = true;
		try {
			// `environment` is a free-form string on the wire; we narrow the
			// dropdown to the three well-known values for now but the wire
			// accepts anything. `description` becomes null when the textarea
			// is empty so the server knows the field is intentionally absent.
			const trimmedDescription = description.trim();
			const arch = await architectureStore.create({
				name: name.trim(),
				description: trimmedDescription.length > 0 ? trimmedDescription : null,
				environment
			});
			goto(`/architectures/${arch.id}`);
		} catch {
			// mutateWithRefresh already toasted the error.
		} finally {
			submitting = false;
		}
	}
</script>

<form class="form" onsubmit={handleSubmit} aria-labelledby="create-arch-heading">
	<h1 id="create-arch-heading" class="form-heading">New architecture</h1>
	<p class="form-subheading">
		Start with a name and an environment. You can sketch the topology in the
		designer once it lands in Phase 2.
	</p>

	<div class="field">
		<label for="arch-name">Name</label>
		<input
			id="arch-name"
			name="name"
			type="text"
			autocomplete="off"
			required
			minlength="3"
			maxlength="64"
			bind:value={name}
			aria-invalid={nameError !== null}
			aria-describedby={nameError ? 'arch-name-error' : 'arch-name-hint'}
		/>
		{#if nameError}
			<p id="arch-name-error" class="field-error">{nameError}</p>
		{:else}
			<p id="arch-name-hint" class="field-hint">
				Used as the slug across YAML and the designer. 3–64 chars.
			</p>
		{/if}
	</div>

	<div class="field">
		<label for="arch-description">Description (optional)</label>
		<textarea
			id="arch-description"
			name="description"
			rows="3"
			maxlength="1024"
			bind:value={description}
		></textarea>
	</div>

	<div class="field">
		<label for="arch-environment">Environment</label>
		<select id="arch-environment" name="environment" bind:value={environment}>
			{#each KNOWN_ARCHITECTURE_ENVIRONMENTS as env (env)}
				<option value={env}>{env}</option>
			{/each}
		</select>
	</div>

	<div class="actions">
		<Button variant="secondary" onclick={() => goto('/architectures')}>Cancel</Button>
		<Button
			variant="primary"
			loading={submitting}
			disabled={!canSubmit}
			type="submit"
			ariaLabel="Create architecture"
		>
			Create architecture
		</Button>
	</div>
</form>

<style>
	.form {
		display: flex;
		flex-direction: column;
		gap: 1rem;
		max-width: 560px;
	}

	.form-heading {
		font-size: var(--chv-text-lg);
		font-weight: 700;
		margin: 0;
	}

	.form-subheading {
		margin: 0 0 0.5rem 0;
		color: var(--chv-color-neutral-600);
		font-size: var(--chv-text-sm);
	}

	.field {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
	}

	.field label {
		font-size: var(--chv-text-xs);
		font-weight: 600;
		color: var(--chv-color-neutral-700);
	}

	.field input,
	.field textarea,
	.field select {
		padding: 0.5rem 0.65rem;
		font-size: var(--chv-text-sm);
		border: 1px solid var(--chv-color-neutral-300);
		border-radius: var(--radius-xs);
		background: var(--bg-surface);
		color: var(--chv-color-neutral-900);
		font-family: inherit;
	}

	.field input:focus,
	.field textarea:focus,
	.field select:focus {
		outline: 2px solid var(--color-primary);
		outline-offset: 1px;
		border-color: var(--color-primary);
	}

	.field input[aria-invalid='true'] {
		border-color: var(--color-danger);
	}

	.field-hint {
		margin: 0;
		font-size: 11px;
		color: var(--chv-color-neutral-500);
	}

	.field-error {
		margin: 0;
		font-size: 11px;
		color: var(--color-danger);
	}

	.actions {
		display: flex;
		gap: 0.5rem;
		justify-content: flex-end;
		margin-top: 0.5rem;
	}
</style>
