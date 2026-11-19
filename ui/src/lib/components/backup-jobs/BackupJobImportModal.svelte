<script lang="ts">
  import Button from '#lib/components/primitives/Button.svelte';
  import Modal from '#lib/components/primitives/Modal.svelte';

  interface Props {
    open: boolean;
    importName: string;
    importFile: File | null;
    importing: boolean;
    handleFileSelect: (e: Event) => void;
    handleImportVM: () => void;
  }

  let {
    open = $bindable(),
    importName = $bindable(),
    importFile,
    importing,
    handleFileSelect,
    handleImportVM
  }: Props = $props();
</script>

<Modal bind:open title="INGEST_DURABLE_WORKLOAD">
  <div class="registry-form">
    <div class="protocol-hint">
       <span>PROTOCOL: WORKLOAD_INGESTION_V1</span>
       <p>Verify artifact checksum before initiating transmission.</p>
    </div>

    <div class="form-group">
      <label for="vm-name">INGEST_IDENTIFIER</label>
      <input id="vm-name" type="text" bind:value={importName} placeholder="e.g. IMPORT_VECT-4" />
    </div>

    <div class="form-group">
      <label for="import-file">SOURCE_ARTIFACT (.qcow2, .ova)</label>
      <input id="import-file" type="file" onchange={handleFileSelect} class="file-ingest" />
    </div>
  </div>

  {#snippet footer()}
    <Button variant="secondary" onclick={() => open = false}>CANCEL</Button>
    <Button variant="primary" onclick={handleImportVM} disabled={importing || !importFile || !importName}>
      {importing ? 'TRANSMITTING...' : 'INITIATE_INGESTION'}
    </Button>
  {/snippet}
</Modal>

<style>
  .registry-form {
    display: flex;
    flex-direction: column;
    gap: 1.25rem;
  }

  .form-group {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }

  .form-group label {
    font-size: 9px;
    font-weight: 800;
    color: var(--chv-color-neutral-500);
    letter-spacing: 0.1em;
  }

  .form-group input {
    background: var(--bg-surface-muted);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-xs);
    padding: 0.5rem;
    font-size: 11px;
    font-weight: 700;
    color: var(--chv-color-neutral-900);
  }

  .protocol-hint {
    background: rgba(var(--color-primary-rgb), 0.1);
    border-left: 2px solid var(--color-primary);
    padding: 0.75rem;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .protocol-hint span { font-size: 9px; font-weight: 800; color: var(--color-primary); }
  .protocol-hint p { font-size: 10px; color: var(--chv-color-neutral-600); margin: 0; }

  .file-ingest {
    padding: 2rem !important;
    border: 1px dashed var(--border-subtle) !important;
    text-align: center;
    cursor: pointer;
  }
</style>
