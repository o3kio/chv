<script lang="ts">
  import { ShieldCheck, Activity } from 'lucide-svelte';
  import SectionCard from '#lib/components/shell/SectionCard.svelte';
  import type { BackupHistory } from '#lib/bff/types.ts';

  interface Props {
    backupHistory: BackupHistory[];
    formatBytes: (bytes: number) => string;
  }

  let { backupHistory, formatBytes }: Props = $props();
</script>

<aside class="support-area">
  <SectionCard title="SLA Integrity" icon={ShieldCheck}>
    <div class="registry-vitals">
       <div class="vital-row">
          <span>RPO_TARGET</span>
          <span>24_HOURS</span>
       </div>
       <div class="vital-row">
          <span>STORAGE_POOL</span>
          <span>DURABLE_S3</span>
       </div>
       <div class="vital-row">
          <span>LAST_CONSISTENCY</span>
          <span>NOMINAL</span>
       </div>
    </div>
  </SectionCard>

  <SectionCard title="Recent Sequences" icon={Activity}>
    <div class="micro-trace-list">
      {#each backupHistory.slice(0, 3) as trace}
        <div class="trace-card">
          <span class="trace-vm">{trace.vm_id}</span>
          <span class="trace-meta">{trace.status} · {formatBytes(trace.size_bytes ?? 0)}</span>
        </div>
      {:else}
         <p class="empty-hint">No operational traces found.</p>
      {/each}
    </div>
  </SectionCard>
</aside>

<style>
  .support-area {
    display: flex;
    flex-direction: column;
    gap: 1rem;
  }

  .registry-vitals {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }

  .vital-row {
    display: flex;
    justify-content: space-between;
    font-size: 9px;
    font-weight: 800;
    color: var(--chv-color-neutral-500);
    padding: 0.35rem 0.5rem;
    background: var(--bg-surface-muted);
    border-radius: var(--radius-xs);
  }

  .vital-row span:last-child { color: var(--chv-color-neutral-900); }

  .micro-trace-list {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }

  .trace-card {
    display: flex;
    flex-direction: column;
    padding: 0.5rem 0.75rem;
    background: var(--bg-surface-muted);
    border-radius: var(--radius-xs);
    gap: 2px;
  }

  .trace-vm { font-size: 10px; font-weight: 800; color: var(--chv-color-neutral-900); }
  .trace-meta { font-size: 9px; font-weight: 700; color: var(--chv-color-neutral-400); text-transform: uppercase; }

  .empty-hint { font-size: 10px; font-weight: 700; color: var(--chv-color-neutral-400); text-align: center; padding: 1rem; }
</style>
