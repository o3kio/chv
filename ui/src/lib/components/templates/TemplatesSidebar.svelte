<script lang="ts">
  import { ArrowRight } from 'lucide-svelte';
  import SectionCard from '#lib/components/shell/SectionCard.svelte';
</script>

<aside class="support-area">
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

  .empty-hint {
    font-size: 11px;
    color: var(--chv-color-neutral-400);
    padding: 1rem;
    text-align: center;
  }
</style>
