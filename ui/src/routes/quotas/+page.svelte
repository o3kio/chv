<script lang="ts">
import Button from '#lib/components/primitives/Button.svelte';
  import { onMount } from 'svelte';
  import { Cpu, HardDrive, Network, Server, Settings, AlertTriangle, Activity, ShieldCheck, RefreshCcw } from 'lucide-svelte';
  import { createAPIClient, getStoredRole } from '#lib/api/client.ts';
  import { toast } from '#lib/stores/toast.svelte.ts';
  import SectionCard from '#lib/components/shell/SectionCard.svelte';
  import CompactMetricCard from '#lib/components/shared/CompactMetricCard.svelte';
  import StatusBadge from '#lib/components/shell/StatusBadge.svelte';
  import PageHeaderWithAction from '#lib/components/shell/PageHeaderWithAction.svelte';
  import ErrorState from '#lib/components/shell/ErrorState.svelte';
  import EmptyInfrastructureState from '#lib/components/shell/EmptyInfrastructureState.svelte';
  import QuotaSettingsModal from '#lib/components/settings/QuotaSettingsModal.svelte';
  import { getPageDefinition } from '#lib/shell/app-shell.ts';
  import type { UsageWithQuota, Quota, UserInfo } from '#lib/api/types.ts';

  const client = createAPIClient();
  const pageDef = getPageDefinition('/overview'); // Reusing overview as it contains quota context

  let usageData = $state<UsageWithQuota | null>(null);
  let allQuotas = $state<Quota[]>([]);
  let users = $state<UserInfo[]>([]);
  let loading = $state(true);
  let error = $state<string | null>(null);
  let showSettingsModal = $state(false);
  let editingQuota = $state<Quota | null>(null);
  let isAdmin = $state(getStoredRole() === 'admin');

  const resources = [
    { key: 'vms', label: 'WORKLOAD_INSTANCES', icon: Server },
    { key: 'cpus', label: 'COMPUTE_CORES', icon: Cpu },
    { key: 'memory_gb', label: 'MEMORY_ALLOCATION', icon: HardDrive, unit: 'GB' },
    { key: 'storage_gb', label: 'STORAGE_ALLOCATION', icon: HardDrive, unit: 'GB' },
    { key: 'networks', label: 'NETWORK_REGISTRY', icon: Network },
  ] as const;

  type ResourceKey = typeof resources[number]['key'];

  async function loadQuotaData() {
    loading = true;
    error = null;
    try {
      usageData = await client.getUsage();
    } catch (err: any) {
      error = err.message || 'Usage registry unavailable';
    } finally {
      loading = false;
    }
  }

  function getUsageValue(key: ResourceKey): number {
    if (!usageData) return 0;
    return usageData.usage[key as keyof typeof usageData.usage] as number;
  }

  function handleModalSuccess() {}

  function getQuotaValue(key: ResourceKey): number {
    if (!usageData) return 0;
    switch (key) {
      case 'vms': return usageData.quota.max_vms;
      case 'cpus': return usageData.quota.max_cpu;
      case 'memory_gb': return usageData.quota.max_memory_gb;
      case 'storage_gb': return usageData.quota.max_storage_gb;
      case 'networks': return usageData.quota.max_networks;
      default: return 0;
    }
  }

  function getPercentage(key: ResourceKey): number {
    const used = getUsageValue(key);
    const max = getQuotaValue(key);
    if (max === 0) return 0;
    return Math.min(100, Math.round((used / max) * 100));
  }

  onMount(loadQuotaData);
</script>

<div class="inventory-page">
  <PageHeaderWithAction page={pageDef}>
    {#snippet actions()}
      <div class="header-actions">
        {#if isAdmin}
          <Button variant="secondary" onclick={() => { editingQuota = usageData?.quota || null; showSettingsModal = true; }}>
            <Settings size={14} />
            Adjust Policy
          </Button>
        {/if}
        <Button variant="primary" onclick={loadQuotaData}>
          <RefreshCcw size={14} />
          Sync Registry
        </Button>
      </div>
    {/snippet}
  </PageHeaderWithAction>

  {#if loading && !usageData}
    <div class="skeleton-grid"></div>
  {:else if error}
    <ErrorState />
  {:else if !usageData}
     <EmptyInfrastructureState title="Policy records missing" description="Unable to locate resource pressure audit logs." />
  {:else}
    <div class="inventory-metrics">
      <CompactMetricCard label="Compute Load" value={getPercentage('cpus') + '%'} color={getPercentage('cpus') > 80 ? 'warning' : 'primary'} />
      <CompactMetricCard label="Memory Load" value="{getPercentage('memory_gb')}%" color={getPercentage('memory_gb') > 80 ? 'warning' : 'primary'} />
      <CompactMetricCard label="Storage Load" value="{getPercentage('storage_gb')}%" color={getPercentage('storage_gb') > 80 ? 'warning' : 'primary'} />
      <CompactMetricCard label="Network Assets" value={String(getUsageValue('networks'))} color="neutral" />
    </div>

    <main class="inventory-main">
      <section class="quota-grid">
        {#each resources as resource}
          {@const used = getUsageValue(resource.key)}
          {@const max = getQuotaValue(resource.key)}
          {@const p = getPercentage(resource.key)}
          <div class="quota-block" class:critical={p >= 90}>
            <div class="quota-block__header">
              <span class="label">{resource.label}</span>
              <span class="p-val" class:high={p > 80}>{p}%</span>
            </div>
            <div class="quota-block__main">
               <div class="val-pair">
                  <span class="used">{used}</span>
                  <span class="sep">/</span>
                  <span class="limit">{max}{(resource as any).unit || ''}</span>
               </div>
               <div class="progress-track">
                  <div class="progress-fill" style="width: {p}%"></div>
               </div>
            </div>
          </div>
        {/each}
      </section>

      <aside class="support-area">
        <SectionCard title="Limit Compliance" icon={ShieldCheck}>
          <div class="audit-summary">
            <div class="summary-row">
              <span>Policy Status</span>
              <span>NOMINAL</span>
            </div>
            <div class="summary-row">
              <span>Oversubscription</span>
              <span>1.2x</span>
            </div>
          </div>
        </SectionCard>

        <SectionCard title="Pressure Audit" icon={Activity}>
          {#if resources.some(r => getPercentage(r.key) > 80)}
            <div class="pressure-list">
               {#each resources.filter(r => getPercentage(r.key) > 80) as r}
                 <div class="pressure-alert">
                    <AlertTriangle size={12} />
                    <span>{r.label} CAPACITY CRITICAL</span>
                 </div>
               {/each}
            </div>
          {:else}
             <p class="empty-hint">All resource families reporting nominal headroom.</p>
          {/if}
        </SectionCard>
      </aside>
    </main>
  {/if}
</div>

<QuotaSettingsModal
  bind:open={showSettingsModal}
  quota={editingQuota}
  {users}
  onSuccess={loadQuotaData}
/>

<style>
  .inventory-page {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }

  .header-actions {
    display: flex;
    gap: 0.5rem;
  }

  .inventory-metrics {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
    gap: 0.75rem;
  }

  .inventory-main {
    display: grid;
    grid-template-columns: 1fr 300px;
    gap: 1rem;
    align-items: start;
  }

  .quota-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(280px, 1fr));
    gap: 0.75rem;
  }

  .quota-block {
    background: var(--bg-surface);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-xs);
    padding: 1rem;
    display: flex;
    flex-direction: column;
    gap: 1rem;
    transition: transform 0.1s ease;
  }

  .quota-block:hover {
    transform: translateY(-1px);
    border-color: var(--color-primary-light);
  }

  .quota-block.critical {
    border-color: var(--color-danger);
    background: rgba(var(--color-danger-rgb), 0.02);
  }

  .quota-block__header {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
  }

  .quota-block__header .label {
    font-size: 10px;
    font-weight: 800;
    color: var(--chv-color-neutral-500);
    letter-spacing: 0.05em;
  }

  .quota-block__header .p-val {
    font-size: 14px;
    font-weight: 800;
    color: var(--chv-color-neutral-900);
    font-family: var(--chv-font-mono);
  }

  .quota-block__header .p-val.high {
    color: var(--color-warning);
  }

  .quota-block.critical .p-val {
    color: var(--color-danger);
  }

  .quota-block__main {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }

  .val-pair {
    display: flex;
    align-items: baseline;
    gap: 0.25rem;
    font-family: var(--chv-font-mono);
  }

  .val-pair .used {
    font-size: 20px;
    font-weight: 800;
    color: var(--chv-color-neutral-900);
  }

  .val-pair .sep {
    font-size: 14px;
    color: var(--chv-color-neutral-400);
  }

  .val-pair .limit {
    font-size: 14px;
    font-weight: 700;
    color: var(--chv-color-neutral-500);
  }

  .progress-track {
    height: 4px;
    background: var(--bg-surface-muted);
    border-radius: 2px;
    overflow: hidden;
  }

  .progress-fill {
    height: 100%;
    background: var(--color-primary);
    border-radius: 2px;
    transition: width 0.3s ease;
  }

  .quota-block.critical .progress-fill {
    background: var(--color-danger);
  }

  .support-area {
    display: flex;
    flex-direction: column;
    gap: 1rem;
  }

  .pressure-alert {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.5rem;
    background: rgba(var(--color-danger-rgb), 0.1);
    color: var(--color-danger);
    font-size: 9px;
    font-weight: 800;
    border-radius: 4px;
  }

  .audit-summary {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }

  .summary-row {
    display: flex;
    justify-content: space-between;
    font-size: 10px;
    color: var(--chv-color-neutral-600);
    padding: 0.35rem 0.5rem;
    background: var(--bg-surface-muted);
    border-radius: var(--radius-xs);
  }

  .summary-row span:last-child {
    font-weight: 700;
    color: var(--chv-color-neutral-900);
  }

  .empty-hint {
    font-size: 11px;
    color: var(--chv-color-neutral-400);
    padding: 1rem;
    text-align: center;
  }

  @media (max-width: 1100px) {
    .inventory-main {
      grid-template-columns: 1fr;
    }
  }
</style>

<!-- Quota Settings Modal -->
<QuotaSettingsModal
  bind:open={showSettingsModal}
  quota={editingQuota}
  {users}
  onSuccess={handleModalSuccess}
/>
