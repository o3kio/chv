<script lang="ts">
	import Button from '#lib/components/primitives/Button.svelte';
	import Modal from '#lib/components/primitives/Modal.svelte';
	import FindingItem from './FindingItem.svelte';
	import { architectureStore } from '#lib/stores/architecture-store.svelte.ts';
	import { toast } from '#lib/stores/toast.svelte.ts';
	import type { Finding, ValidationResult } from '#lib/bff/architectures.ts';

	interface Props {
		open: boolean;
		/**
		 * Async submit handler. Receives the raw YAML string and is expected
		 * to perform the actual persistence (create + import, or plain import
		 * against an existing architecture). Should resolve with the final
		 * ValidationResult so the dialog can surface the outcome and close.
		 */
		onSubmit: (yaml: string) => Promise<ValidationResult>;
		onClose: () => void;
	}

	let { open = $bindable(false), onSubmit, onClose }: Props = $props();

	let yamlText = $state('');
	let validateBeforeImport = $state(true);
	let validating = $state(false);
	let submitting = $state(false);
	let preview = $state<ValidationResult | null>(null);
	let fileError = $state<string | null>(null);

	const SEVERITY_ORDER: Record<Finding['severity'], number> = {
		error: 0,
		warning: 1,
		info: 2
	};

	const sortedPreviewFindings = $derived.by<Finding[]>(() => {
		if (!preview) return [];
		return [...preview.findings].sort((a, b) => {
			const sev = SEVERITY_ORDER[a.severity] - SEVERITY_ORDER[b.severity];
			if (sev !== 0) return sev;
			return a.code.localeCompare(b.code);
		});
	});

	const blockedByPreview = $derived(
		validateBeforeImport && preview !== null && preview.status === 'invalid'
	);
	const canSubmit = $derived(yamlText.trim().length > 0 && !submitting && !blockedByPreview);

	async function handleFileChange(event: Event) {
		const target = event.target as HTMLInputElement;
		const file = target.files?.[0];
		if (!file) return;
		fileError = null;
		try {
			const text = await file.text();
			yamlText = text;
		} catch (err) {
			fileError = err instanceof Error ? err.message : 'Could not read file';
		} finally {
			// Reset so re-uploading the same file fires `change` again.
			target.value = '';
		}
	}

	async function handleValidate() {
		if (validating || !yamlText.trim()) return;
		validating = true;
		preview = null;
		try {
			preview = await architectureStore.validateYaml(yamlText);
		} catch (err) {
			toast.error(err instanceof Error ? err.message : 'Validation failed');
		} finally {
			validating = false;
		}
	}

	async function handleSubmit() {
		if (!canSubmit) return;
		submitting = true;
		try {
			if (validateBeforeImport && preview === null) {
				// Honour the toggle even if the user never clicked Validate first.
				const result = await architectureStore.validateYaml(yamlText);
				preview = result;
				if (result.status === 'invalid') {
					toast.error('YAML has blocking errors — fix them or disable "Validate before import".');
					return;
				}
			}
			const result = await onSubmit(yamlText);
			if (result.status === 'invalid') {
				// Persist succeeded but server still flagged errors. Keep the
				// dialog open so the user can see them; do NOT close.
				preview = result;
				toast.error('YAML imported with blocking errors — see findings.');
				return;
			}
			toast.success('YAML imported successfully');
			reset();
			onClose();
		} catch (err) {
			// onSubmit / mutateWithRefresh already toasted on its own path; we
			// only handle the case where the parent rethrew an unexpected error.
			if (err instanceof Error && !err.message.toLowerCase().includes('failed')) {
				toast.error(err.message);
			}
		} finally {
			submitting = false;
		}
	}

	function reset() {
		yamlText = '';
		preview = null;
		fileError = null;
		validateBeforeImport = true;
	}

	function handleClose() {
		if (submitting) return;
		reset();
		onClose();
	}
</script>

<Modal {open} title="Import YAML" width="wide" onClose={handleClose}>
	<div class="dialog-body" data-testid="import-yaml-dialog">
		<p class="hint">
			Paste a topology YAML or upload a <code>.yaml</code> / <code>.yml</code> file.
			The server validates against the schema and policy registry before persisting.
		</p>

		<label class="field">
			<span class="field-label">YAML</span>
			<textarea
				class="yaml-textarea"
				rows="12"
				placeholder="kind: Topology&#10;name: my-topology&#10;..."
				bind:value={yamlText}
				oninput={() => (preview = null)}
				aria-label="YAML content"
				data-testid="import-yaml-textarea"
			></textarea>
		</label>

		<div class="row">
			<label class="file-upload">
				<input
					type="file"
					accept=".yaml,.yml,text/yaml,application/x-yaml"
					onchange={handleFileChange}
					data-testid="import-yaml-file"
				/>
				<span>Upload file…</span>
			</label>

			<label class="toggle">
				<input
					type="checkbox"
					bind:checked={validateBeforeImport}
					data-testid="import-yaml-toggle-validate"
				/>
				<span>Validate before import</span>
			</label>
		</div>

		{#if fileError}
			<p class="error-text" role="alert">{fileError}</p>
		{/if}

		{#if preview}
			<div class="preview" data-testid="import-yaml-preview">
				<div class="preview-head">
					<span
						class="status-pill status-{preview.status}"
						aria-label={`Validation status: ${preview.status}`}
						data-testid="import-yaml-status"
					>
						{preview.status}
					</span>
					<span class="counts">
						{preview.summary.errors} errors · {preview.summary.warnings} warnings · {preview.summary.info} info
					</span>
				</div>
				{#if sortedPreviewFindings.length > 0}
					<ul class="preview-list">
						{#each sortedPreviewFindings as finding (`${finding.code}|${finding.path}|${finding.message}`)}
							<FindingItem {finding} />
						{/each}
					</ul>
				{/if}
			</div>
		{/if}
	</div>

	{#snippet footer()}
		<Button variant="secondary" size="sm" onclick={handleClose} disabled={submitting}>
			Cancel
		</Button>
		<Button
			variant="ghost"
			size="sm"
			loading={validating}
			disabled={yamlText.trim().length === 0 || submitting}
			onclick={handleValidate}
			ariaLabel="Validate YAML"
		>
			Validate
		</Button>
		<Button
			variant="primary"
			size="sm"
			loading={submitting}
			disabled={!canSubmit}
			onclick={handleSubmit}
			ariaLabel="Import YAML"
		>
			Import
		</Button>
	{/snippet}
</Modal>

<style>
	.dialog-body {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}

	.hint {
		margin: 0;
		font-size: 12px;
		color: var(--chv-color-neutral-600);
	}

	.field {
		display: flex;
		flex-direction: column;
		gap: 0.3rem;
	}

	.field-label {
		font-size: 11px;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--chv-color-neutral-700);
	}

	.yaml-textarea {
		width: 100%;
		font-family: var(--chv-font-mono, ui-monospace, monospace);
		font-size: 12px;
		line-height: 1.5;
		padding: 0.6rem;
		border: 1px solid var(--chv-color-neutral-300);
		border-radius: var(--radius-xs);
		background: var(--chv-color-neutral-50, #f8fafc);
		color: var(--chv-color-neutral-900);
		resize: vertical;
	}

	.yaml-textarea:focus {
		outline: 2px solid var(--color-primary);
		outline-offset: 1px;
		border-color: var(--color-primary);
	}

	.row {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 0.75rem;
		flex-wrap: wrap;
	}

	.file-upload {
		display: inline-flex;
		align-items: center;
		gap: 0.4rem;
		font-size: 12px;
		color: var(--chv-color-neutral-700);
		cursor: pointer;
	}

	.file-upload input[type='file'] {
		font-size: 12px;
	}

	.toggle {
		display: inline-flex;
		align-items: center;
		gap: 0.4rem;
		font-size: 12px;
		color: var(--chv-color-neutral-700);
		cursor: pointer;
	}

	.toggle input {
		cursor: pointer;
	}

	.error-text {
		margin: 0;
		font-size: 12px;
		color: var(--color-danger);
	}

	.preview {
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
		padding: 0.75rem;
		border: 1px solid var(--chv-color-neutral-200);
		border-radius: var(--radius-xs);
		background: var(--chv-color-neutral-50, #f8fafc);
	}

	.preview-head {
		display: flex;
		align-items: center;
		gap: 0.5rem;
	}

	.status-pill {
		display: inline-block;
		padding: 0.1rem 0.5rem;
		font-size: 11px;
		font-weight: 700;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		border-radius: var(--radius-xs);
		color: white;
	}

	.status-valid {
		background: #15803d;
	}

	.status-warning {
		background: #b45309;
	}

	.status-invalid {
		background: #b91c1c;
	}

	.counts {
		font-size: 12px;
		color: var(--chv-color-neutral-600);
	}

	.preview-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
		max-height: 240px;
		overflow: auto;
	}
</style>
