<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { goto } from '$app/navigation';
  import { Search, X, Clock, Server, Image, Network, FileText } from 'lucide-svelte';
  import { 
    openSearch, 
    closeSearch, 
    searchDebounced, 
    addToRecentSearches,
    clearRecentSearches,
    getGroupedResults,
    typeLabels,
    typeIcons,
    highlightMatches,
    getIsOpen,
    getSearchQuery,
    setSearchQuery,
    getRecentSearches,
    type SearchResult,
    type SearchItemType,
    type SearchItem
  } from '#lib/stores/search.svelte.ts';
  
  // Props
  interface Props {
    open?: boolean;
  }
  
  let { open = $bindable(false) }: Props = $props();
  
  // Local state
  let results = $state<SearchResult[]>([]);
  let selectedIndex = $state(0);
  let inputRef = $state<HTMLInputElement | null>(null);
  let resultsContainerRef = $state<HTMLDivElement | null>(null);
  let isVisible = $state(false);
  let isClosing = $state(false);
  
  // Derived state
  let query = $derived(getSearchQuery());
  let recent = $derived(getRecentSearches());
  let hasQuery = $derived(query.trim().length > 0);
  let groupedResults = $derived(getGroupedResults(results));
  let allItems = $derived(getAllItems());
  
  // Build flat list of items for keyboard navigation
  function getAllItems(): (SearchResult | { item: SearchItem; isRecent: true; score: number })[] {
    if (hasQuery) {
      return results;
    }
    return recent.map(r => ({ item: r, isRecent: true as const, score: 0 }));
  }
  
  // Get grouped entries as array for iteration
  let groupedEntries = $derived(Array.from(groupedResults.entries()));
  
  // Compute global indices for grouped results
  function getGlobalIndex(type: SearchItemType, indexInGroup: number): number {
    let count = 0;
    for (const [t, typeResults] of groupedEntries) {
      if (t === type) {
        return count + indexInGroup;
      }
      count += typeResults.length;
    }
    return 0;
  }
  
  // Sync local open state to global store (one-direction only)
  $effect(() => {
    if (open !== getIsOpen()) {
      if (open) openSearch();
      else closeSearch();
    }
  });
  
  // Watch for query changes
  function handleInput(event: Event) {
    const value = (event.target as HTMLInputElement).value;
    setSearchQuery(value);
    selectedIndex = 0;
    
    if (value.trim()) {
      searchDebounced(value, (searchResults) => {
        results = searchResults;
      });
    } else {
      results = [];
    }
  }
  
  // Handle keyboard navigation
  function handleKeyDown(event: KeyboardEvent) {
    switch (event.key) {
      case 'Escape':
        event.preventDefault();
        handleClose();
        break;
        
      case 'ArrowDown':
        event.preventDefault();
        selectedIndex = Math.min(selectedIndex + 1, allItems.length - 1);
        scrollToSelected();
        break;
        
      case 'ArrowUp':
        event.preventDefault();
        selectedIndex = Math.max(selectedIndex - 1, 0);
        scrollToSelected();
        break;
        
      case 'Enter':
        event.preventDefault();
        const selected = allItems[selectedIndex];
        if (selected) {
          selectItem(selected.item);
        }
        break;
        
      case 'Home':
        event.preventDefault();
        selectedIndex = 0;
        scrollToSelected();
        break;
        
      case 'End':
        event.preventDefault();
        selectedIndex = allItems.length - 1;
        scrollToSelected();
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
  
  function selectItem(item: SearchItem) {
    addToRecentSearches(item);
    handleClose();
    
    if (item.route) {
      goto(item.route);
    }
  }
  
  function handleClose() {
    if (isClosing) return;
    isClosing = true;
    
    setTimeout(() => {
      isVisible = false;
      open = false;
      closeSearch();
      results = [];
      selectedIndex = 0;
      setSearchQuery('');
      isClosing = false;
    }, 150);
  }
  
  function handleBackdropClick(event: MouseEvent) {
    if (event.target === event.currentTarget) {
      handleClose();
    }
  }
  
  function getIconComponent(type: SearchItemType) {
    switch (type) {
      case 'vm': return Server;
      case 'image': return Image;
      case 'network': return Network;
      case 'page': return FileText;
      default: return FileText;
    }
  }
  
  function getIconColor(type: SearchItemType): string {
    switch (type) {
      case 'vm': return 'text-blue-500';
      case 'image': return 'text-purple-500';
      case 'network': return 'text-green-500';
      case 'page': return 'text-(--shell-text-secondary)';
      default: return 'text-(--shell-text-secondary)';
    }
  }
  
  // Reset visibility when opening
  $effect(() => {
    if (open && !isVisible) {
      tick().then(() => {
        isVisible = true;
        inputRef?.focus();
      });
    }
  });
  
  onMount(() => {
    if (open) {
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
      aria-label="Global search"
      class="w-full max-w-2xl mx-4 bg-(--bg-surface) rounded-lg shadow-2xl overflow-hidden transition-all duration-150"
      class:scale-95={!isVisible || isClosing}
      class:scale-100={isVisible && !isClosing}
      class:opacity-0={!isVisible || isClosing}
      class:opacity-100={isVisible && !isClosing}
      onclick={(e) => e.stopPropagation()}
    >
      <!-- Search Input -->
      <div class="flex items-center gap-3 px-4 py-3 border-b border-(--border-subtle)">
        <Search size={20} class="text-(--shell-text-muted) shrink-0" />
        <input
          bind:this={inputRef}
          type="text"
          value={query}
          oninput={handleInput}
          onkeydown={handleKeyDown}
          placeholder="Search VMs, images, networks..."
          class="flex-1 bg-transparent text-base outline-hidden placeholder:text-(--shell-text-muted)"
          aria-label="Search"
          aria-autocomplete="list"
          aria-controls="search-results"
          aria-activedescendant={allItems.length > 0 ? `search-item-${selectedIndex}` : undefined}
        />
        {#if query}
          <button
            type="button"
            onclick={() => { setSearchQuery(''); results = []; inputRef?.focus(); }}
            class="p-1 rounded-sm hover:bg-(--bg-surface-muted) text-(--shell-text-muted) hover:text-(--shell-text-secondary)"
            aria-label="Clear search"
          >
            <X size={16} />
          </button>
        {/if}
        <kbd class="hidden sm:inline-flex items-center gap-1 px-2 py-1 text-xs font-mono bg-(--bg-surface-muted) text-(--shell-text-secondary) rounded-sm border border-(--border-subtle)">
          ESC
        </kbd>
      </div>
      
      <!-- Results Container -->
      <div 
        bind:this={resultsContainerRef}
        id="search-results"
        class="max-h-[60vh] overflow-y-auto"
        role="listbox"
        aria-label="Search results"
      >
        {#if hasQuery}
          <!-- Search Results -->
          {#if results.length === 0}
            <div class="px-4 py-8 text-center text-(--shell-text-secondary)">
              <p>No results found for "{query}"</p>
              <p class="text-sm mt-1 text-(--shell-text-muted)">Try a different search term</p>
            </div>
          {:else}
            {#each groupedEntries as [type, typeResults]}
              <div class="py-2">
                <div class="px-4 py-1.5 text-xs font-semibold text-(--shell-text-secondary) uppercase tracking-wider bg-(--bg-surface-muted)">
                  {typeLabels[type]}
                </div>
                {#each typeResults as result, i}
                  {@const globalIndex = getGlobalIndex(type, i)}
                  {@const Icon = getIconComponent(type)}
                  <button
                    type="button"
                    id="search-item-{globalIndex}"
                    data-index={globalIndex}
                    role="option"
                    aria-selected={selectedIndex === globalIndex}
                    class="w-full px-4 py-2.5 flex items-center gap-3 text-left hover:bg-(--bg-surface-muted) transition-colors"
                    class:bg-[var(--color-primary-light)]={selectedIndex === globalIndex}
                    onclick={() => selectItem(result.item)}
                  >
                    <Icon size={18} class={getIconColor(type)} />
                    <div class="flex-1 min-w-0">
                      <div class="text-sm font-medium text-(--shell-text) truncate">
                        {@html highlightMatches(result.item.name, result.matches, 'name')}
                      </div>
                      {#if result.item.description}
                        <div class="text-xs text-(--shell-text-secondary) truncate">
                          {@html highlightMatches(result.item.description, result.matches, 'description')}
                        </div>
                      {/if}
                    </div>
                    {#if result.item.route}
                      <span class="text-xs text-(--shell-text-muted) hidden sm:block">→</span>
                    {/if}
                  </button>
                {/each}
              </div>
            {/each}
          {/if}
        {:else if recent.length > 0}
          <!-- Recent Searches -->
          <div class="py-2">
            <div class="px-4 py-1.5 flex items-center justify-between">
              <span class="text-xs font-semibold text-(--shell-text-secondary) uppercase tracking-wider">Recent</span>
              <button
                type="button"
                onclick={clearRecentSearches}
                class="text-xs text-(--shell-text-muted) hover:text-(--shell-text-secondary)"
              >
                Clear
              </button>
            </div>
            {#each recent as item, i}
              {@const Icon = getIconComponent(item.type)}
              <button
                type="button"
                id="search-item-{i}"
                data-index={i}
                role="option"
                aria-selected={selectedIndex === i}
                class="w-full px-4 py-2.5 flex items-center gap-3 text-left hover:bg-(--bg-surface-muted) transition-colors"
                class:bg-[var(--color-primary-light)]={selectedIndex === i}
                onclick={() => selectItem(item)}
              >
                <Clock size={18} class="text-(--shell-text-muted)" />
                <Icon size={18} class={getIconColor(item.type)} />
                <div class="flex-1 min-w-0">
                  <div class="text-sm font-medium text-(--shell-text) truncate">{item.name}</div>
                  {#if item.description}
                    <div class="text-xs text-(--shell-text-secondary) truncate">{item.description}</div>
                  {/if}
                </div>
                <span class="text-xs text-(--shell-text-muted) hidden sm:block">{typeLabels[item.type]}</span>
              </button>
            {/each}
          </div>
        {:else}
          <!-- Empty state with suggestions -->
          <div class="px-4 py-6">
            <p class="text-sm text-(--shell-text-secondary) mb-3">Type to search or try:</p>
            <div class="flex flex-wrap gap-2">
              <kbd class="px-2 py-1 text-xs bg-(--bg-surface-muted) text-(--shell-text-secondary) rounded-sm border border-(--border-subtle)">vm-</kbd>
              <kbd class="px-2 py-1 text-xs bg-(--bg-surface-muted) text-(--shell-text-secondary) rounded-sm border border-(--border-subtle)">image-</kbd>
              <kbd class="px-2 py-1 text-xs bg-(--bg-surface-muted) text-(--shell-text-secondary) rounded-sm border border-(--border-subtle)">network-</kbd>
            </div>
          </div>
        {/if}
      </div>
      
      <!-- Footer -->
      <div class="px-4 py-2 bg-(--bg-surface-muted) border-t border-(--border-subtle) flex items-center justify-between text-xs text-(--shell-text-secondary)">
        <div class="flex items-center gap-3">
          <span class="flex items-center gap-1">
            <kbd class="px-1.5 py-0.5 bg-(--bg-surface) rounded-sm border border-(--border-subtle)">↑↓</kbd>
            <span>Navigate</span>
          </span>
          <span class="flex items-center gap-1">
            <kbd class="px-1.5 py-0.5 bg-(--bg-surface) rounded-sm border border-(--border-subtle)">↵</kbd>
            <span>Select</span>
          </span>
        </div>
        <div>
          <span>{allItems.length} item{allItems.length !== 1 ? 's' : ''}</span>
        </div>
      </div>
    </div>
  </div>
{/if}

<style>
  :global(.search-highlight) {
    background-color: rgba(59, 130, 246, 0.2);
    color: rgb(29, 78, 216);
    font-weight: 500;
    border-radius: 2px;
    padding: 0 1px;
  }
</style>
