<script lang="ts">
  import { ShieldCheck, ArrowRight } from 'lucide-svelte';
  import SectionCard from '#lib/components/shell/SectionCard.svelte';
</script>

<aside class="support-area">
  <SectionCard title="Library Insights" icon={ShieldCheck}>
    <div class="audit-summary">
      <div class="summary-row">
        <span>Scan Status</span>
        <span>CLEAN</span>
      </div>
      <div class="summary-row">
        <span>Auto-Sync</span>
        <span>ENABLED</span>
      </div>
    </div>
  </SectionCard>

  <SectionCard title="Directives" icon={ArrowRight}>
    <p class="empty-hint">Blueprint library optimized for fabric placement acceleration.</p>
  </SectionCard>
</aside>

<style>
  .support-area {
    display: flex;
    flex-direction: column;
    gap: 1rem;
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
</style>
