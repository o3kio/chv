<script lang="ts">
  import { onMount, tick } from 'svelte';
  import {
    Zap,
    X
  } from 'lucide-svelte';
  import Fuse from 'fuse.js';
  import { buildQuickActions, type QuickAction } from './quick-actions-data';
  import QuickActionsResults from './QuickActionsResults.svelte';
  import QuickActionsFooter from './QuickActionsFooter.svelte';

  // Props
  interface Props {
    open?: boolean;
    onClose?: () => void;
  }

  let { open = $bindable(false), onClose }: Props = $props();

  // Local state
  let query = $state('');
  let selectedIndex = $state(0);
  let isVisible = $state(false);
  let isClosing = $state(false);
  let inputRef = $state<HTMLInputElement | null>(null);
  let resultsContainerRef = $state<HTMLDivElement | null>(null);
  let recentlyUsed = $state<string[]>([]);

  const RECENT_ACTIONS_KEY = 'chv-recent-actions';
  const MAX_RECENT = 5;

  // All available actions
  const allActions: QuickAction[] = buildQuickActions(() => close());

  // Fuse for fuzzy search
  const fuse = new Fuse(allActions, {
    keys: [
      { name: 'title', weight: 0.5 },
      { name: 'description', weight: 0.3 },
      { name: 'keywords', weight: 0.2 }
    ],
    threshold: 0.4
  });

  // Filtered results
  let filteredActions = $derived(getFilteredActions());

  function getFilteredActions(): QuickAction[] {
    if (!query.trim()) {
      // Show recently used first, then other actions
      const recent = recentlyUsed
        .map(id => allActions.find(a => a.id === id))
        .filter(Boolean) as QuickAction[];

      const others = allActions.filter(a => !recentlyUsed.includes(a.id));
      return [...recent, ...others];
    }

    const results = fuse.search(query);
    return results.map(r => r.item);
  }

  // Group actions by section
  let groupedActions = $derived(getGroupedActions(filteredActions));

  function getGroupedActions(actions: QuickAction[]): Map<string, QuickAction[]> {
    const grouped = new Map<string, QuickAction[]>();

    for (const action of actions) {
      if (!grouped.has(action.section)) {
        grouped.set(action.section, []);
      }
      grouped.get(action.section)!.push(action);
    }

    return grouped;
  }

  // Get flat list for keyboard navigation
  let flatActions = $derived(getFlatActions(groupedActions));

  function getFlatActions(grouped: Map<string, QuickAction[]>): QuickAction[] {
    const flat: QuickAction[] = [];
    for (const actions of grouped.values()) {
      flat.push(...actions);
    }
    return flat;
  }

  // Load recent actions
  function loadRecentActions() {
    if (typeof localStorage === 'undefined') return;
    try {
      const stored = localStorage.getItem(RECENT_ACTIONS_KEY);
      if (stored) {
        recentlyUsed = JSON.parse(stored);
      }
    } catch {
      recentlyUsed = [];
    }
  }

  // Save to recent
  function addToRecent(actionId: string) {
    recentlyUsed = [actionId, ...recentlyUsed.filter(id => id !== actionId)].slice(0, MAX_RECENT);
    if (typeof localStorage !== 'undefined') {
      try {
        localStorage.setItem(RECENT_ACTIONS_KEY, JSON.stringify(recentlyUsed));
      } catch {
        // Ignore
      }
    }
  }

  // Handle action selection
  function selectAction(action: QuickAction) {
    addToRecent(action.id);
    close();
    tick().then(() => {
      action.action();
    });
  }

  // Handle input
  function handleInput(event: Event) {
    query = (event.target as HTMLInputElement).value;
    selectedIndex = 0;
  }

  // Handle keyboard navigation
  function handleKeyDown(event: KeyboardEvent) {
    switch (event.key) {
      case 'Escape':
        event.preventDefault();
        close();
        break;

      case 'ArrowDown':
        event.preventDefault();
        selectedIndex = Math.min(selectedIndex + 1, flatActions.length - 1);
        scrollToSelected();
        break;

      case 'ArrowUp':
        event.preventDefault();
        selectedIndex = Math.max(selectedIndex - 1, 0);
        scrollToSelected();
        break;

      case 'Enter':
        event.preventDefault();
        const action = flatActions[selectedIndex];
        if (action) {
          selectAction(action);
        }
        break;
    }
  }

  function scrollToSelected() {
    tick().then(() => {
      const selectedEl = resultsContainerRef?.querySelector(`[data-index="${selectedIndex}"]`);
      if (selectedEl) {
        selectedEl.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
      }
    });
  }

  // Close handlers
  function close() {
    if (isClosing) return;
    isClosing = true;

    setTimeout(() => {
      isVisible = false;
      open = false;
      query = '';
      selectedIndex = 0;
      isClosing = false;
      onClose?.();
    }, 150);
  }

  function handleBackdropClick(event: MouseEvent) {
    if (event.target === event.currentTarget) {
      close();
    }
  }

  // Get global index for an action
  function getGlobalIndex(sectionIndex: number, actionIndex: number): number {
    let count = 0;
    const sections = Array.from(groupedActions.entries());

    for (let i = 0; i < sectionIndex; i++) {
      count += sections[i][1].length;
    }

    return count + actionIndex;
  }

  // Watch for open state
  $effect(() => {
    if (open && !isVisible) {
      loadRecentActions();
      tick().then(() => {
        isVisible = true;
        inputRef?.focus();
      });
    }
  });

  onMount(() => {
    if (open) {
      loadRecentActions();
      tick().then(() => inputRef?.focus());
    }
  });
</script>

{#if open}
  <!-- Backdrop -->
  <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
  <div
    class="fixed inset-0 z-50 bg-black/50 flex items-start justify-center pt-[15vh] transition-opacity duration-150"
    class:opacity-0={!isVisible || isClosing}
    class:opacity-100={isVisible && !isClosing}
    onclick={handleBackdropClick}
    aria-hidden="true"
  >
    <!-- Modal Container -->
    <div
      role="dialog"
      aria-modal="true"
      tabindex="-1"
      aria-label="Quick actions"
      class="w-full max-w-xl mx-4 bg-white rounded-lg shadow-2xl overflow-hidden transition-all duration-150"
      class:scale-95={!isVisible || isClosing}
      class:scale-100={isVisible && !isClosing}
      class:opacity-0={!isVisible || isClosing}
      class:opacity-100={isVisible && !isClosing}
      onclick={(e) => e.stopPropagation()}
    >
      <!-- Header -->
      <div class="flex items-center gap-3 px-4 py-3 border-b border-gray-200">
        <Zap size={20} class="text-amber-500" />
        <input
          bind:this={inputRef}
          type="text"
          value={query}
          oninput={handleInput}
          onkeydown={handleKeyDown}
          placeholder="What would you like to do?"
          class="flex-1 bg-transparent text-base outline-hidden placeholder:text-gray-400"
          aria-label="Quick action search"
        />
        {#if query}
          <button
            type="button"
            onclick={() => { query = ''; inputRef?.focus(); }}
            class="p-1 rounded-sm hover:bg-gray-100 text-gray-400 hover:text-gray-600"
            aria-label="Clear search"
          >
            <X size={16} />
          </button>
        {/if}
        <kbd class="hidden sm:inline-flex items-center gap-1 px-2 py-1 text-xs font-mono bg-gray-100 text-gray-500 rounded-sm border border-gray-200">
          ESC
        </kbd>
      </div>

      <!-- Results -->
      <div
        bind:this={resultsContainerRef}
        class="max-h-[50vh] overflow-y-auto"
        role="listbox"
      >
        <QuickActionsResults
          {flatActions}
          {groupedActions}
          {selectedIndex}
          {query}
          {selectAction}
          {getGlobalIndex}
        />
      </div>

      <!-- Footer -->
      <QuickActionsFooter />
    </div>
  </div>
{/if}
