<script lang="ts">
  import { Copy, FileCode } from 'lucide-svelte';
  import InventoryTable from '#lib/components/shell/InventoryTable.svelte';
  import StatusBadge from '#lib/components/shell/StatusBadge.svelte';
  import ErrorState from '#lib/components/shell/ErrorState.svelte';
  import type { ShellTone } from '#lib/shell/app-shell.ts';
  import type { VMTemplate, CloudInitTemplate, Image } from '#lib/api/types.ts';

  type Column = { key: string; label: string; align?: 'left' | 'right' | 'center' };

  interface Props {
    activeTab: 'vm' | 'cloudinit';
    loading: boolean;
    error: string;
    vmTemplates: VMTemplate[];
    cloudInitTemplates: CloudInitTemplate[];
    images: Image[];
    vmColumns: Column[];
    ciColumns: Column[];
    cloneTemplate: (template: VMTemplate) => void;
  }

  let {
    activeTab,
    loading,
    error,
    vmTemplates,
    cloudInitTemplates,
    images,
    vmColumns,
    ciColumns,
    cloneTemplate
  }: Props = $props();
</script>

<section class="inventory-table-area">
  {#if loading && vmTemplates.length === 0}
    <div class="skeleton-table"></div>
  {:else if error}
    <ErrorState />
  {:else if activeTab === 'vm'}
    <InventoryTable columns={vmColumns} rows={vmTemplates.map(t => ({
      ...t,
      resources: `${t.vcpu} vCPU / ${t.memory_mb}MB`,
      image_name: images.find(i => i.id === t.image_id)?.name || t.image_id,
      status: { label: 'VERIFIED', tone: 'healthy' }
    }))}>
      {#snippet cell({ column, row })}
         {#if column.key === 'name'}
           <span class="blueprint-name">{row.name}</span>
         {:else if column.key === 'status'}
           <StatusBadge label={row.status.label} tone={row.status.tone as ShellTone} />
         {:else if column.key === '_actions'}
           <div class="row-ops">
              <button type="button" class="op-btn" onclick={() => cloneTemplate(row)} title="Orchestrate Workload" aria-label="Clone template {row.name}"><Copy size={12} /></button>
           </div>
         {:else}
           <span class="cell-text">{(row as Record<string, unknown>)[column.key]}</span>
         {/if}
      {/snippet}
    </InventoryTable>
  {:else}
    <InventoryTable columns={ciColumns} rows={cloudInitTemplates.map(t => ({
      ...t,
      variables: t.variables?.join(', ') || 'NONE'
    }))}>
       {#snippet cell({ column, row })}
         {#if column.key === 'name'}
           <span class="blueprint-name">{row.name}</span>
         {:else if column.key === '_actions'}
           <div class="row-ops">
              <button type="button" class="op-btn" title="View Registry" aria-label="View registry for {row.name}"><FileCode size={12} /></button>
           </div>
         {:else}
           <span class="cell-text">{(row as Record<string, unknown>)[column.key]}</span>
         {/if}
      {/snippet}
    </InventoryTable>
  {/if}
</section>

<style>
  .blueprint-name {
    font-weight: 700;
    color: var(--chv-color-neutral-900);
  }

  .row-ops {
    display: flex;
    gap: 0.25rem;
  }

  .op-btn {
    width: 24px;
    height: 24px;
    display: grid;
    place-items: center;
    border-radius: 4px;
    color: var(--chv-color-neutral-500);
    transition: all 0.1s ease;
  }

  .op-btn:hover {
    background: var(--bg-surface-muted);
    color: var(--color-primary);
  }
</style>
