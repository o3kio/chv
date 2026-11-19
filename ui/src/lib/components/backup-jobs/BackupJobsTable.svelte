<script lang="ts">
  import { Play, Pause, Download } from 'lucide-svelte';
  import InventoryTable from '#lib/components/shell/InventoryTable.svelte';
  import StatusBadge from '#lib/components/shell/StatusBadge.svelte';
  import ErrorState from '#lib/components/shell/ErrorState.svelte';
  import type { BackupJob, BackupHistory } from '#lib/bff/types.ts';

  type Column = { key: string; label: string; align?: 'left' | 'right' | 'center' };

  interface Props {
    activeTab: 'jobs' | 'history';
    loading: boolean;
    error: string;
    backupJobs: BackupJob[];
    backupHistory: BackupHistory[];
    jobColumns: Column[];
    historyColumns: Column[];
    formatBytes: (bytes: number) => string;
    runJobNow: (job: BackupJob) => void;
    toggleJob: (job: BackupJob) => void;
  }

  let {
    activeTab,
    loading,
    error,
    backupJobs,
    backupHistory,
    jobColumns,
    historyColumns,
    formatBytes,
    runJobNow,
    toggleJob
  }: Props = $props();
</script>

<section class="inventory-table-area">
  {#if loading && backupJobs.length === 0}
    <div class="discovery-loading">Syncing protection metadata...</div>
  {:else if error}
    <ErrorState description={error} />
  {:else if activeTab === 'jobs'}
    <InventoryTable
      columns={jobColumns}
      rows={backupJobs.map(j => ({
        ...j,
        created_at: j.created_at ? new Date(j.created_at).toLocaleDateString() : '—'
      }))}
    >
      {#snippet cell({ column, row })}
         {#if column.key === 'job_id'}
           <div class="registry-identity">
             <span class="p-name">{row.backup_type}</span>
             <span class="p-id">ID // {row.job_id.slice(0,8)}</span>
           </div>
         {:else if column.key === 'status'}
           <StatusBadge label={row.status.toUpperCase()} tone={row.status === 'Enabled' || row.status === 'Completed' ? 'healthy' : row.status === 'Pending' ? 'warning' : 'degraded'} />
         {:else if column.key === '_actions'}
            <div class="op-cluster">
               <button type="button" class="op-ctrl" onclick={() => runJobNow(row)} title="FORCE_EXECUTE" aria-label="Run job now"><Play size={12} /></button>
               <button type="button" class="op-ctrl" onclick={() => toggleJob(row)} title="TOGGLE_STATUS" aria-label={row.status === 'Enabled' ? 'Pause job' : 'Start job'}>
                  {#if row.status === 'Enabled'}<Pause size={12} />{:else}<Play size={12} />{/if}
               </button>
            </div>
         {:else}
           <span class="cell-text">{(row as Record<string, unknown>)[column.key] ?? '—'}</span>
         {/if}
      {/snippet}
    </InventoryTable>
  {:else}
    <InventoryTable columns={historyColumns} rows={backupHistory.map(h => ({
      ...h,
      size: formatBytes(h.size_bytes ?? 0),
      started_at: h.started_at ? new Date(h.started_at).toLocaleString() : '—'
    }))}>
      {#snippet cell({ column, row })}
         {#if column.key === 'status'}
           <StatusBadge label={row.status.toUpperCase()} tone={row.status === 'Completed' ? 'healthy' : row.status === 'Pending' ? 'warning' : 'failed'} />
         {:else if column.key === 'completed_at'}
           <div class="trace-end">
             <span class="timestamp">{row.completed_at ? new Date(String(row.completed_at)).toLocaleTimeString() : '—'}</span>
             <button type="button" class="trace-dl" title="DOWNLOAD_ARTIFACT" aria-label="Download backup artifact"><Download size={12} /></button>
           </div>
         {:else}
           <span class="cell-text">{(row as Record<string, unknown>)[column.key] ?? '—'}</span>
         {/if}
      {/snippet}
    </InventoryTable>
  {/if}
</section>

<style>
  .registry-identity {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .p-name { font-weight: 800; color: var(--chv-color-neutral-900); font-size: 11px; }
  .p-id { font-size: 9px; font-weight: 700; color: var(--chv-color-neutral-400); font-family: var(--chv-font-mono); }

  .cell-text { font-size: 11px; color: var(--chv-color-neutral-600); }

  .op-cluster {
    display: flex;
    gap: 0.25rem;
  }

  .op-ctrl {
    width: 24px;
    height: 24px;
    display: grid;
    place-items: center;
    background: var(--bg-surface-muted);
    border: 1px solid var(--border-subtle);
    border-radius: 2px;
    color: var(--chv-color-neutral-500);
    cursor: pointer;
  }

  .op-ctrl:hover {
    background: var(--bg-surface);
    color: var(--color-primary);
    border-color: var(--color-primary);
  }

  .trace-end {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 0.75rem;
  }

  .timestamp { font-size: 10px; font-family: var(--chv-font-mono); color: var(--chv-color-neutral-500); }
  .trace-dl {
     background: transparent;
     border: none;
     color: var(--chv-color-neutral-400);
     cursor: pointer;
  }
  .trace-dl:hover { color: var(--color-primary); }

  .discovery-loading {
    padding: 4rem;
    text-align: center;
    font-size: 10px;
    font-weight: 800;
    color: var(--chv-color-neutral-400);
    letter-spacing: 0.1em;
  }
</style>
