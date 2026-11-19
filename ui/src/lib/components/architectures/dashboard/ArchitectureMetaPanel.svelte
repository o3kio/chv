<script lang="ts">
	import Button from '#lib/components/primitives/Button.svelte';
	import { architectureStore, StaleVersionError } from '#lib/stores/architecture-store.svelte.ts';
	import {
		KNOWN_ARCHITECTURE_ENVIRONMENTS,
		type Architecture
	} from '#lib/bff/architectures.ts';

	interface Props {
		architecture: Architecture;
		onUpdated: (next: Architecture) => void;
		onStaleVersion: () => void;
	}

	let { architecture, onUpdated, onStaleVersion }: Props = $props();

	let editing = $state(false);
	let saving = $state(false);
	// Drafts are component-local $state. We deliberately do NOT reset them on a
	// StaleVersionError so the user does not lose typing on conflict (PR review
	// finding M5). The drafts are also intentionally NOT a $derived view of
	// `architecture` — a fresh server copy after Reload must not silently
	// overwrite in-flight typing.
	let draftDisplayName = $state('');
	let draftDescription = $state('');
	let draftEnvironment = $state('');
	// `draftsDirty` flips to true the moment the user starts editing and stays
	// true until they explicitly Cancel or a Save succeeds. This lets us
	// preserve their typing across a Reload (the parent swaps `architecture`
	// for the fresh server copy after a stale-version conflict): we leave edit
	// mode so the refreshed metadata is visible, but on the next click of Edit
	// we re-seed from the server only if the drafts have NOT been touched.
	let draftsDirty = $state(false);
	// Track the architecture identity we're editing against. If the parent
	// loader replaces `architecture` with a fresh server copy (Reload after a
	// stale-version banner), we drop out of edit mode so the user sees the
	// new metadata; their drafts are preserved by `draftsDirty` above.
	let editingId = $state<string | null>(null);
	let editingVersion = $state<number | null>(null);

	$effect(() => {
		if (
			editing &&
			!saving &&
			editingId === architecture.id &&
			editingVersion !== null &&
			architecture.version_number !== editingVersion
		) {
			// External version bump while in edit mode — typically a Reload
			// after a stale-version conflict. Exit edit mode but keep drafts
			// (draftsDirty stays true), so re-clicking Edit shows the user's
			// in-flight text instead of overwriting it from the server copy.
			editing = false;
		}
	});

	function startEdit() {
		// Only seed drafts from the server when there is no in-flight edit to
		// preserve. M5: a user who lost a save to a stale-version conflict
		// should keep their typing when they re-open the editor.
		if (!draftsDirty) {
			draftDisplayName = architecture.display_name ?? architecture.name;
			draftDescription = architecture.description ?? '';
			draftEnvironment = architecture.environment ?? '';
		}
		editingId = architecture.id;
		editingVersion = architecture.version_number;
		editing = true;
	}

	function markDirty() {
		draftsDirty = true;
	}

	function cancelEdit() {
		editing = false;
		// Cancel discards drafts on purpose — the user has chosen not to save.
		draftsDirty = false;
	}

	async function saveEdit() {
		if (saving) return;
		saving = true;
		markDirty();
		try {
			const next = await architectureStore.update(
				architecture.id,
				architecture.version_number,
				{
					display_name: draftDisplayName.trim(),
					description: draftDescription.trim(),
					// Empty string -> null so the server clears the column rather
					// than storing an empty environment label.
					environment: draftEnvironment.trim().length > 0 ? draftEnvironment.trim() : null
				}
			);
			onUpdated(next);
			editing = false;
			draftsDirty = false;
		} catch (err) {
			if (err instanceof StaleVersionError) {
				// PR review M5: preserve the user's draft on conflict so they
				// don't lose their typing. We stay in edit mode and DO NOT reset
				// `draftDisplayName/draftDescription/draftEnvironment`. The
				// parent renders the StaleVersionBanner; once the user clicks
				// Reload, the $effect above flips us out of edit mode so the
				// fresh server metadata becomes visible, but the drafts (and
				// `draftsDirty`) survive so the next Edit click re-opens the
				// user's in-flight text.
				onStaleVersion();
			}
			// Other errors are already toasted by mutateWithRefresh.
		} finally {
			saving = false;
		}
	}

	function formatDate(iso: string | null): string {
		if (!iso) return '—';
		try {
			return new Date(iso).toLocaleString();
		} catch {
			return iso;
		}
	}
</script>

<section class="panel" aria-labelledby="meta-heading" data-testid="architecture-meta-panel">
	<header class="panel-header">
		<h2 id="meta-heading" class="panel-title">Metadata</h2>
		{#if !editing}
			<Button variant="secondary" size="sm" onclick={startEdit} ariaLabel="Edit metadata">
				Edit
			</Button>
		{/if}
	</header>

	{#if editing}
		<div class="form" data-testid="architecture-meta-edit-form">
			<div class="field">
				<label for="meta-display-name">Display name</label>
				<input
					id="meta-display-name"
					type="text"
					bind:value={draftDisplayName}
					oninput={markDirty}
					maxlength="128"
					required
				/>
			</div>
			<div class="field">
				<label for="meta-description">Description</label>
				<textarea
					id="meta-description"
					bind:value={draftDescription}
					oninput={markDirty}
					rows="3"
					maxlength="1024"
				></textarea>
			</div>
			<div class="field">
				<label for="meta-environment">Environment</label>
				<select id="meta-environment" bind:value={draftEnvironment} onchange={markDirty}>
					<option value="">— none —</option>
					{#each KNOWN_ARCHITECTURE_ENVIRONMENTS as env (env)}
						<option value={env}>{env}</option>
					{/each}
					{#if draftEnvironment && !KNOWN_ARCHITECTURE_ENVIRONMENTS.includes(draftEnvironment as (typeof KNOWN_ARCHITECTURE_ENVIRONMENTS)[number])}
						<option value={draftEnvironment}>{draftEnvironment}</option>
					{/if}
				</select>
			</div>
			<div class="actions">
				<Button variant="secondary" size="sm" onclick={cancelEdit}>Cancel</Button>
				<Button
					variant="primary"
					size="sm"
					loading={saving}
					onclick={saveEdit}
					ariaLabel="Save metadata"
				>
					Save
				</Button>
			</div>
		</div>
	{:else}
		<dl class="meta-grid">
			<div class="meta-row">
				<dt>Name</dt>
				<dd data-testid="meta-name">{architecture.display_name ?? architecture.name}</dd>
			</div>
			<div class="meta-row">
				<dt>Slug</dt>
				<dd data-testid="meta-slug">{architecture.name}</dd>
			</div>
			<div class="meta-row">
				<dt>Description</dt>
				<dd data-testid="meta-description">{architecture.description ?? '—'}</dd>
			</div>
			<div class="meta-row">
				<dt>Environment</dt>
				<dd data-testid="meta-environment">{architecture.environment ?? '—'}</dd>
			</div>
			<div class="meta-row">
				<dt>Status</dt>
				<dd data-testid="meta-status">{architecture.status}</dd>
			</div>
			<div class="meta-row">
				<dt>Version</dt>
				<dd data-testid="meta-version">{architecture.version_number}</dd>
			</div>
			<div class="meta-row">
				<dt>Created</dt>
				<dd>{formatDate(architecture.created_at)}</dd>
			</div>
			<div class="meta-row">
				<dt>Updated</dt>
				<dd>{formatDate(architecture.updated_at)}</dd>
			</div>
		</dl>
	{/if}
</section>

<style>
	.panel {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
		padding: 1rem;
		background: var(--bg-surface);
		border: 1px solid var(--chv-color-neutral-200);
		border-radius: var(--radius-sm);
	}

	.panel-header {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 0.5rem;
	}

	.panel-title {
		font-size: var(--chv-text-sm);
		font-weight: 700;
		margin: 0;
		color: var(--chv-color-neutral-700);
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}

	.meta-grid {
		display: grid;
		grid-template-columns: 1fr;
		gap: 0.5rem;
		margin: 0;
	}

	.meta-row {
		display: grid;
		grid-template-columns: 140px 1fr;
		gap: 0.75rem;
		font-size: var(--chv-text-sm);
	}

	.meta-row dt {
		color: var(--chv-color-neutral-500);
		font-weight: 600;
	}

	.meta-row dd {
		margin: 0;
		color: var(--chv-color-neutral-900);
		word-break: break-word;
	}

	.form {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}

	.field {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
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

	.actions {
		display: flex;
		gap: 0.5rem;
		justify-content: flex-end;
	}
</style>
