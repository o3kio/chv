<script lang="ts">
  import { Activity } from 'lucide-svelte';
  import SectionCard from '#lib/components/shell/SectionCard.svelte';
  import type { BackupHistory } from '#lib/bff/types.ts';

  interface Props {
    backupHistory: BackupHistory[];
    formatBytes: (bytes: number) => string;
  }

  let { backupHistory, formatBytes }: Props = $props();
</script>

<aside class="support-area">
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
