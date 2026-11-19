<script lang="ts">
  import Button from '#lib/components/primitives/Button.svelte';
  import Modal from '#lib/components/primitives/Modal.svelte';
  import type { VmListItem } from '#lib/bff/types.ts';

  interface Props {
    open: boolean;
    vms: VmListItem[];
    selectedVMId: string;
    selectedBackupType: string;
    selectedTargetPath: string;
    selectedStorageBackend: string;
    creatingJob: boolean;
    handleCreateJob: () => void;
  }

  let {
    open = $bindable(),
    vms,
    selectedVMId = $bindable(),
    selectedBackupType = $bindable(),
    selectedTargetPath = $bindable(),
    selectedStorageBackend = $bindable(),
    creatingJob,
    handleCreateJob
  }: Props = $props();
</script>

<Modal bind:open title="DEFINE_PROTECTION_POLICY">
  <div class="registry-form">
    <div class="form-group">
      <label for="vm-select">TARGET_COMPUTE_NODE</label>
      <select id="vm-select" bind:value={selectedVMId}>
        <option value="">SELECT_WORKLOAD...</option>
        {#each vms as vm}
          <option value={vm.vm_id}>{vm.name} // {vm.cpu} {vm.memory}</option>
        {/each}
      </select>
    </div>

    <div class="form-group">
      <label for="backup-type">BACKUP_TYPE</label>
      <select id="backup-type" bind:value={selectedBackupType}>
        <option value="full">FULL</option>
        <option value="incremental">INCREMENTAL</option>
        <option value="snapshot">SNAPSHOT</option>
      </select>
    </div>

    <div class="form-group">
      <label for="target-path">TARGET_PATH (optional)</label>
      <input id="target-path" type="text" bind:value={selectedTargetPath} placeholder="e.g. /backups/vm-daily" />
    </div>

    <div class="form-group">
      <label for="storage-backend">STORAGE_BACKEND (optional)</label>
      <select id="storage-backend" bind:value={selectedStorageBackend}>
        <option value="">DEFAULT</option>
        <option value="local">LOCAL</option>
        <option value="s3">S3</option>
        <option value="nfs">NFS</option>
      </select>
    </div>
  </div>

  {#snippet footer()}
    <Button variant="secondary" onclick={() => open = false}>CANCEL</Button>
    <Button variant="primary" onclick={handleCreateJob} disabled={creatingJob || !selectedVMId}>
      {creatingJob ? 'COMMITTING...' : 'COMMIT_POLICY'}
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

  .form-group input,
  .form-group select {
    background: var(--bg-surface-muted);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-xs);
    padding: 0.5rem;
    font-size: 11px;
    font-weight: 700;
    color: var(--chv-color-neutral-900);
  }
</style>
