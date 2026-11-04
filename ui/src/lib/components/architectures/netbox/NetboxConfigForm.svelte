<script lang="ts">
	import Button from '#lib/components/primitives/Button.svelte';
	import type { NetboxConfig, NetboxRetentionPolicy } from '#lib/bff/architectures.ts';
	import type { NetboxConfigDraft } from '#lib/stores/architecture-netbox-store.svelte.ts';

	/**
	 * NetBox projection config form.
	 *
	 * The form is dumb (no BFF calls): the panel passes `onSave` /
	 * `onDelete` backed by the store. Drafts are component-local $state so
	 * a StaleVersionError conflict leaves the operator's typing intact
	 * (same contract ArchitectureMetaPanel documents).
	 *
	 * Token write-only semantics: the token field is type=password,
	 * starts empty on every load, and an empty field means "keep the
	 * existing secret" — the token key is OMITTED from the submitted
	 * draft, and the value is cleared immediately after dispatch.
	 */

	interface Props {
		architectureId: string;
		config: NetboxConfig | null;
		expectedVersion: number;
		saving: boolean;
		deleting: boolean;
		onSave: (draft: NetboxConfigDraft) => Promise<void> | void;
		onDelete: () => void;
	}

	let {
		architectureId,
		config,
		expectedVersion,
		saving,
		deleting,
		onSave,
		onDelete
	}: Props = $props();

	let draftEndpoint = $state('');
	let draftToken = $state('');
	let draftSecretRef = $state('');
	let draftRetention: NetboxRetentionPolicy = $state('mark_stale');
	let draftPostApply = $state(false);
	let draftSiteName = $state('');
	// `draftsDirty` flips true on first input and stays true until a
	// successful save, so a fresh config never overwrites in-flight typing.
	let draftsDirty = $state(false);
	let seededId = $state<string | null>(null);

	function seed() {
		draftEndpoint = config?.endpoint ?? '';
		draftToken = '';
		draftSecretRef = config?.token_secret_ref ?? `netbox-${architectureId}`;
		draftRetention = config?.retention_policy ?? 'mark_stale';
		draftPostApply = config?.enable_post_apply ?? false;
		draftSiteName = config?.site_name ?? '';
		draftsDirty = false;
	}

	// Seed drafts from the loaded config: once per architecture switch
	// (forced), then again whenever a fresh config object arrives — as
	// long as the operator has not started typing.
	$effect(() => {
		if (seededId !== architectureId) {
			seededId = architectureId;
			draftsDirty = false;
			seed();
			return;
		}
		if (!draftsDirty) seed();
	});

	function markDirty() {
		draftsDirty = true;
	}

	const tokenPlaceholder = $derived(
		config?.token_set
			? 'Token set — leave blank to keep the existing token'
			: 'NetBox API token (required on first save)');

	const tokenHelp = $derived(
		config?.token_set
			? 'Write-only: submitted once, never displayed again. Leave blank to keep the stored token.'
			: 'Write-only: encrypted at rest, never returned by the API.');

	async function handleSubmit(event: SubmitEvent) {
		event.preventDefault();
		if (saving) return;
		const draft: NetboxConfigDraft = {
			endpoint: draftEndpoint.trim(),
			// Write-only semantics: an empty field omits the key entirely,
			// which the BFF treats as "keep the existing secret".
			...(draftToken.trim().length > 0 ? { token: draftToken.trim() } : {}),
			token_secret_ref: draftSecretRef.trim(),
			retention_policy: draftRetention,
			enable_post_apply: draftPostApply,
			site_name: draftSiteName.trim().length > 0 ? draftSiteName.trim() : null
		};
		// Clear the transient token draft before dispatching — the value
		// has left the component and must not linger in local state.
		draftToken = '';
		try {
			await onSave(draft);
			// Success: re-seed from the freshly saved summary and drop the
			// dirty flag so the form reflects the server's truth.
			seed();
		} catch {
			// Stale-version conflict or BFF error: keep the drafts (minus
			// the already-cleared token) so the operator can retry.
		}
	}

	function handleDelete() {
		if (deleting) return;
		onDelete();
	}
</script>

<form class="form" onsubmit={handleSubmit} data-testid="netbox-config-form">
	<div class="form-heading">
		<h3 class="form-title">NetBox connection</h3>
		{#if config}
			<span class="form-hint" data-testid="netbox-config-updated">Updated {new Date(config.updated_at).toLocaleString()}</span>
		{:else}
			<span class="form-hint" data-testid="netbox-config-absent">No projection config yet — saving creates one.</span>
		{/if}
	</div>

	<label class="field">
		<span class="field-label">NetBox endpoint</span>
		<input
			id="netbox-endpoint-input"
			class="field-input"
			type="url"
			placeholder="https://netbox.example.internal"
			bind:value={draftEndpoint}
			oninput={markDirty}
			required
			data-testid="netbox-endpoint-input"
			aria-describedby="netbox-endpoint-help"
		/>
		<span id="netbox-endpoint-help" class="field-help">HTTPS is required — plain HTTP endpoints are rejected.</span>
	</label>

	<label class="field">
		<span class="field-label">API token</span>
		<input
			id="netbox-token-input"
			class="field-input"
			type="password"
			autocomplete="new-password"
			placeholder={tokenPlaceholder}
			bind:value={draftToken}
			oninput={markDirty}
			data-testid="netbox-token-input"
			aria-describedby="netbox-token-help"
		/>
		<span id="netbox-token-help" class="field-help" data-testid="netbox-token-help">{tokenHelp}</span>
	</label>

	<label class="field">
		<span class="field-label">Token secret reference</span>
		<input
			id="netbox-secret-ref-input"
			class="field-input"
			type="text"
			bind:value={draftSecretRef}
			oninput={markDirty}
			required
			data-testid="netbox-secret-ref-input"
		/>
		<span class="field-help">Label for the encrypted secret in the audit trail.</span>
	</label>

	<label class="field">
		<span class="field-label">Retention policy</span>
		<select
			id="netbox-retention-input"
			class="field-input"
			bind:value={draftRetention}
			onchange={markDirty}
			data-testid="netbox-retention-input"
			aria-describedby="netbox-retention-help"
		>
			<option value="mark_stale">Mark stale (default)</option>
			<option value="delete">Delete removed objects</option>
		</select>
		<span id="netbox-retention-help" class="field-help">
			{#if draftRetention === 'delete'}
				Deleted NetBox objects are gone for good — the delete policy requires an Admin to save.
			{:else}
				Removed CHV resources are tagged stale in NetBox; nothing is ever deleted.
			{/if}
		</span>
	</label>

	<label class="field field-inline">
		<input
			id="netbox-post-apply-input"
			type="checkbox"
			bind:checked={draftPostApply}
			onchange={markDirty}
			data-testid="netbox-post-apply-input"
			aria-describedby="netbox-post-apply-help"
		/>
		<span class="field-label">Export after every successful apply</span>
	</label>
	<span id="netbox-post-apply-help" class="field-help">Enqueues a projection run after every successful apply (best-effort — a NetBox outage never changes the apply result).</span>

	<label class="field">
		<span class="field-label">NetBox site (optional)</span>
		<input
			id="netbox-site-input"
			class="field-input"
			type="text"
			placeholder="dc1"
			bind:value={draftSiteName}
			oninput={markDirty}
			data-testid="netbox-site-input"
		/>
		<span class="field-help">Site label assigned to projected devices; defaults to the environment label.</span>
	</label>

	{#if config}
		<p class="field-help" data-testid="netbox-custom-field-prefix" title="Managed via the API — this form always omits the field; an omitted field keeps the stored prefix.">
			Custom field prefix <code>{config.custom_field_prefix}</code> — managed via the API; this
			form's saves keep the stored prefix.
		</p>
	{/if}

	<div class="form-actions">
		<Button
			variant="primary"
			size="sm"
			type="submit"
			loading={saving}
			data-testid="netbox-config-save-button"
			ariaLabel={saving ? 'Saving…' : config ? 'Save config' : 'Create config'}
		>
			{saving ? 'Saving…' : config ? 'Save config' : 'Create config'}
		</Button>
		{#if config}
			<Button
				variant="danger"
				size="sm"
				onclick={handleDelete}
				loading={deleting}
				data-testid="netbox-config-delete-button"
				ariaLabel={deleting ? 'Removing…' : 'Remove config'}
			>
				{deleting ? 'Removing…' : 'Remove config'}
			</Button>
		{/if}
		<span class="form-version" data-testid="netbox-config-expected-version">
			(expected version {expectedVersion})
		</span>
	</div>
</form>

<style>
	.form { display: flex; flex-direction: column; gap: 0.75rem; }
	.form-heading { display: flex; align-items: baseline; gap: 0.5rem; flex-wrap: wrap; }
	.form-title {
		margin: 0;
		font-size: var(--text-sm);
		font-weight: 700;
		color: var(--color-neutral-700);
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}
	.form-hint { font-size: 12px; color: var(--color-neutral-500); }
	.field {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		max-width: 480px;
	}
	.field-inline { flex-direction: row; align-items: center; gap: 0.5rem; }
	.field-label { font-size: 12px; font-weight: 600; color: var(--color-neutral-700); }
	.field-input {
		padding: 0.4rem 0.6rem;
		font-size: var(--text-sm);
		border: 1px solid var(--color-neutral-300);
		border-radius: var(--radius-xs);
		background: var(--bg-surface);
		color: var(--color-neutral-900);
		width: 100%;
		box-sizing: border-box;
	}
	.field-input:focus-visible { outline: 2px solid var(--color-primary); outline-offset: 1px; }
	.field-help { font-size: 11px; color: var(--color-neutral-500); line-height: 1.4; }
	.form-actions { display: flex; align-items: center; gap: 0.5rem; flex-wrap: wrap; }
	.form-version { font-size: 11px; color: var(--color-neutral-400); }
</style>
