<script lang="ts">
  import { ShieldAlert } from 'lucide-svelte';
</script>

<svelte:head>
  <title>Backups — not available | CHV</title>
</svelte:head>

<div class="inventory-page">
  <section class="fence-card" aria-labelledby="backup-fence-title">
    <div class="fence-icon" aria-hidden="true">
      <ShieldAlert size={22} />
    </div>
    <div class="fence-copy">
      <p class="eyebrow">Data protection</p>
      <h1 id="backup-fence-title">Backup and restore are not available in this release</h1>
      <p class="lede">
        CHV ships no tenant-data backup execution engine. The backup and restore API
        surface is deliberately fenced and answers <code>501 NOT_IMPLEMENTED</code>;
        no scheduled job can run, and a backup without restore validation is not
        disaster recovery.
      </p>
      <p class="lede">
        Protect VM disks and volumes with the tooling you already operate, and keep the
        control-plane SQLite backup runbook (<code>docs/runbooks/control-plane-dr.md</code>)
        for control-plane recovery. The supported envelope and the full list of operator
        responsibilities are in <code>docs/DEPLOYMENT-ARCHITECTURE.md</code> §7.
      </p>
    </div>
  </section>
</div>

<style>
  .inventory-page {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }

  .fence-card {
    display: flex;
    gap: 1rem;
    align-items: flex-start;
    max-width: 52rem;
    padding: 1.25rem;
    border: 1px solid var(--color-neutral-200);
    border-radius: 0.5rem;
    background: var(--color-neutral-0, #fff);
  }

  .fence-icon {
    display: flex;
    align-items: center;
    justify-content: center;
    flex: 0 0 auto;
    width: 2.5rem;
    height: 2.5rem;
    border-radius: 9999px;
    color: var(--color-warning-600, #b45309);
    background: var(--color-warning-50, #fffbeb);
  }

  .fence-copy {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }

  .eyebrow {
    margin: 0;
    font-size: 0.6875rem;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--color-neutral-500);
  }

  h1 {
    margin: 0;
    font-size: 1.125rem;
    font-weight: 700;
    line-height: 1.3;
  }

  .lede {
    margin: 0;
    font-size: 0.875rem;
    line-height: 1.55;
    color: var(--color-neutral-600);
  }

  code {
    font-size: 0.8125rem;
  }
</style>
