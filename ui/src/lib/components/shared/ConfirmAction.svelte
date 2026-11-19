<script lang="ts">
  import Modal from '#lib/components/primitives/Modal.svelte';
  import Badge from '#lib/components/primitives/Badge.svelte';
  import { AlertTriangle, AlertCircle, Info, Trash2, Power, Play, Square, RotateCcw } from 'lucide-svelte';

  type ActionType = 'start' | 'stop' | 'restart' | 'delete' | 'restore' | 'generic';

  interface ImpactItem {
    label: string;
    value: string | number;
    type?: 'info' | 'warning' | 'danger' | 'success';
  }

  interface Props {
    open?: boolean;
    title: string;
    description?: string;
    actionType?: ActionType;
    confirmText?: string;
    cancelText?: string;
    items?: string[];
    impact?: ImpactItem[];
    requireTypeConfirmation?: boolean;
    typeConfirmationText?: string;
    onConfirm: () => void;
    onCancel?: () => void;
  }

  let {
    open = $bindable(false),
    title,
    description,
    actionType = 'generic',
    confirmText,
    cancelText = 'Cancel',
    items = [],
    impact = [],
    requireTypeConfirmation = false,
    typeConfirmationText = 'confirm',
    onConfirm,
    onCancel
  }: Props = $props();

  let typedConfirmation = $state('');
  let confirmButtonRef = $state<HTMLButtonElement | null>(null);

  const actionConfig = {
    start: {
      icon: Play,
      variant: 'success' as const,
      defaultConfirmText: 'Start',
      buttonClass: 'bg-success text-white hover:bg-success/90'
    },
    stop: {
      icon: Square,
      variant: 'warning' as const,
      defaultConfirmText: 'Stop',
      buttonClass: 'bg-warning text-white hover:bg-warning/90'
    },
    restart: {
      icon: RotateCcw,
      variant: 'warning' as const,
      defaultConfirmText: 'Restart',
      buttonClass: 'bg-warning text-white hover:bg-warning/90'
    },
    delete: {
      icon: Trash2,
      variant: 'danger' as const,
      defaultConfirmText: 'Delete',
      buttonClass: 'bg-danger text-white hover:bg-danger/90'
    },
    restore: {
      icon: RotateCcw,
      variant: 'info' as const,
      defaultConfirmText: 'Restore',
      buttonClass: 'bg-primary text-white hover:bg-primary/90'
    },
    generic: {
      icon: Info,
      variant: 'default' as const,
      defaultConfirmText: 'Confirm',
      buttonClass: 'bg-primary text-white hover:bg-primary/90'
    }
  };

  const config = $derived(actionConfig[actionType]);
  const finalConfirmText = $derived(confirmText ?? config.defaultConfirmText);

  const canConfirm = $derived(
    !requireTypeConfirmation || typedConfirmation.toLowerCase() === typeConfirmationText.toLowerCase()
  );

  function handleConfirm() {
    if (!canConfirm) return;
    open = false;
    typedConfirmation = '';
    onConfirm();
  }

  function handleCancel() {
    open = false;
    typedConfirmation = '';
    onCancel?.();
  }

  function getImpactVariant(type?: string): 'default' | 'success' | 'warning' | 'danger' | 'info' {
    switch (type) {
      case 'danger': return 'danger';
      case 'warning': return 'warning';
      case 'success': return 'success';
      case 'info': return 'info';
      default: return 'default';
    }
  }

  // Focus the confirm button when modal opens
  $effect(() => {
    if (open) {
      requestAnimationFrame(() => {
        confirmButtonRef?.focus();
      });
    }
  });
</script>

<Modal bind:open closeOnBackdrop={true} onClose={handleCancel}>
  {#snippet header()}
    <div class="flex items-center gap-3">
      {#if actionType === 'delete'}
        <AlertTriangle class="h-5 w-5 text-danger" aria-hidden="true" />
      {:else if actionType === 'stop' || actionType === 'restart'}
        <AlertCircle class="h-5 w-5 text-warning" aria-hidden="true" />
      {:else}
        <config.icon class="h-5 w-5 text-primary" aria-hidden="true" />
      {/if}
      <h2 id="modal-title" class="text-base font-semibold text-ink">{title}</h2>
    </div>
  {/snippet}

  <div class="space-y-4">
    {#if description}
      <p class="text-sm text-muted">{description}</p>
    {/if}

    <!-- Items list -->
    {#if items.length > 0}
      <div class="bg-chrome rounded-lg p-3">
        <p class="text-xs font-medium text-muted uppercase tracking-wider mb-2">
          Affected Items ({items.length})
        </p>
        <div class="max-h-32 overflow-y-auto space-y-1">
          {#each items as item}
            <div class="flex items-center gap-2 text-sm text-ink py-1 px-2 bg-white rounded-sm border border-line">
              <config.icon size={14} class="text-muted" />
              <span class="truncate">{item}</span>
            </div>
          {/each}
        </div>
      </div>
    {/if}

    <!-- Impact summary -->
    {#if impact.length > 0}
      <div class="space-y-2">
        <p class="text-xs font-medium text-muted uppercase tracking-wider">Impact Summary</p>
        <div class="grid grid-cols-2 gap-2">
          {#each impact as item}
            <div class="flex items-center justify-between p-2 bg-chrome rounded-sm border border-line">
              <span class="text-xs text-muted">{item.label}</span>
              <Badge variant={getImpactVariant(item.type)}>{item.value}</Badge>
            </div>
          {/each}
        </div>
      </div>
    {/if}

    <!-- Type confirmation for destructive actions -->
    {#if requireTypeConfirmation}
      <div class="space-y-2 pt-2 border-t border-line">
        <p class="text-sm text-muted">
          To confirm, type <code class="bg-chrome px-1.5 py-0.5 rounded-sm text-xs font-mono">{typeConfirmationText}</code> below:
        </p>
        <input
          type="text"
          bind:value={typedConfirmation}
          placeholder={`Type "${typeConfirmationText}" to confirm`}
          class="w-full px-3 py-2 text-sm border border-line rounded-sm focus:outline-hidden focus:ring-2 focus:ring-primary/30 focus:border-primary"
        />
      </div>
    {/if}
  </div>

  {#snippet footer()}
    <button
      type="button"
      onclick={handleCancel}
      class="px-4 py-2 rounded-sm border border-line text-ink bg-white hover:bg-chrome transition-colors text-sm font-medium"
    >
      {cancelText}
    </button>
    <button
      bind:this={confirmButtonRef}
      type="button"
      onclick={handleConfirm}
      disabled={!canConfirm}
      class="px-4 py-2 rounded-sm font-medium transition-all text-sm focus:outline-hidden focus:ring-2 focus:ring-offset-1 disabled:opacity-50 disabled:cursor-not-allowed {config.buttonClass}"
    >
      {finalConfirmText}
    </button>
  {/snippet}
</Modal>
