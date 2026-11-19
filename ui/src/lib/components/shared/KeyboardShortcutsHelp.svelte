<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { X, Search, Command } from 'lucide-svelte';
  import { 
    toggleHelp, 
    isHelpOpen, 
    getShortcutsByContext,
    formatShortcut,
    getModifierKey,
    type ShortcutGroup 
  } from '#lib/stores/keyboard.svelte.ts';
  import KeyboardShortcut from './KeyboardShortcut.svelte';
  import Input from '../primitives/Input.svelte';
  
  // Local state
  let isVisible = $state(false);
  let isClosing = $state(false);
  let searchQuery = $state('');
  let groups = $state<ShortcutGroup[]>([]);
  let inputRef = $state<HTMLInputElement | null>(null);
  
  // Sync with global store
  $effect(() => {
    const storeOpen = isHelpOpen();
    if (storeOpen && !isVisible) {
      tick().then(() => {
        isVisible = true;
        inputRef?.focus();
        groups = getShortcutsByContext();
      });
    }
  });
  
  // Filter shortcuts based on search
  let filteredGroups = $derived(filterGroups(groups, searchQuery));
  
  function filterGroups(groups: ShortcutGroup[], query: string): ShortcutGroup[] {
    if (!query.trim()) return groups;
    
    const lowerQuery = query.toLowerCase();
    return groups
      .map(group => ({
        ...group,
        shortcuts: group.shortcuts.filter(s => 
          s.description.toLowerCase().includes(lowerQuery) ||
          s.key.toLowerCase().includes(lowerQuery) ||
          s.id.toLowerCase().includes(lowerQuery)
        )
      }))
      .filter(group => group.shortcuts.length > 0);
  }
  
  // Context display names
  const contextNames: Record<string, string> = {
    'global': 'Global Shortcuts',
    'navigation': 'Navigation',
    'vms': 'VM List',
    'vm-detail': 'VM Detail'
  };
  
  // Context descriptions
  const contextDescriptions: Record<string, string> = {
    'global': 'Available everywhere',
    'navigation': 'Quick navigation between pages',
    'vms': 'When viewing VM list',
    'vm-detail': 'When viewing a specific VM'
  };
  
  function handleClose() {
    if (isClosing) return;
    isClosing = true;
    
    setTimeout(() => {
      isVisible = false;
      isClosing = false;
      searchQuery = '';
      toggleHelp(false);
    }, 150);
  }
  
  function handleKeyDown(event: KeyboardEvent) {
    if (event.key === 'Escape') {
      event.preventDefault();
      handleClose();
    }
  }
  
  function handleBackdropClick(event: MouseEvent) {
    if (event.target === event.currentTarget) {
      handleClose();
    }
  }
  
  // Parse shortcut key for display
  function parseKeys(shortcut: { key: string; modifiers?: string[] }): string[] {
    const keys: string[] = [];
    
    if (shortcut.modifiers) {
      keys.push(...shortcut.modifiers);
    }
    
    // Handle multi-key sequences
    if (shortcut.key.length > 1 && !shortcut.modifiers?.length) {
      // Split sequence like 'gd' into ['g', 'd']
      keys.push(...shortcut.key.split(''));
    } else {
      keys.push(shortcut.key);
    }
    
    return keys;
  }
</script>

{#if isHelpOpen()}
  <!-- Backdrop -->
  <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
  <div
    class="fixed inset-0 z-50 bg-black/50 flex items-center justify-center p-4 transition-opacity duration-150"
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
      aria-label="Keyboard shortcuts help"
      class="w-full max-w-3xl max-h-[85vh] bg-white rounded-lg shadow-2xl overflow-hidden flex flex-col transition-all duration-150"
      class:scale-95={!isVisible || isClosing}
      class:scale-100={isVisible && !isClosing}
      class:opacity-0={!isVisible || isClosing}
      class:opacity-100={isVisible && !isClosing}
      onclick={(e) => e.stopPropagation()}
    >
      <!-- Header -->
      <div class="flex items-center justify-between px-6 py-4 border-b border-gray-200">
        <div class="flex items-center gap-3">
          <Command size={20} class="text-gray-500" />
          <h2 class="text-lg font-semibold text-gray-900">Keyboard Shortcuts</h2>
        </div>
        <button
          type="button"
          onclick={handleClose}
          class="p-2 rounded-sm hover:bg-gray-100 text-gray-500 hover:text-gray-700 transition-colors"
          aria-label="Close"
        >
          <X size={18} />
        </button>
      </div>
      
      <!-- Search -->
      <div class="px-6 py-3 border-b border-gray-200 bg-gray-50">
        <div class="relative">
          <Search size={16} class="absolute left-3 top-1/2 -translate-y-1/2 text-gray-400" />
          <input
            bind:this={inputRef}
            type="text"
            bind:value={searchQuery}
            onkeydown={handleKeyDown}
            placeholder="Search shortcuts..."
            class="w-full pl-9 pr-4 py-2 text-sm border border-gray-300 rounded-lg focus:outline-hidden focus:ring-2 focus:ring-blue-500 focus:border-transparent"
          />
        </div>
      </div>
      
      <!-- Content -->
      <div class="flex-1 overflow-y-auto p-6">
        {#if filteredGroups.length === 0}
          <div class="text-center py-8 text-gray-500">
            <Command size={48} class="mx-auto mb-3 opacity-30" />
            <p>No shortcuts found for "{searchQuery}"</p>
            <p class="text-sm mt-1 text-gray-400">Try a different search term</p>
          </div>
        {:else}
          <div class="space-y-6">
            {#each filteredGroups as group}
              <section>
                <div class="mb-3">
                  <h3 class="text-sm font-semibold text-gray-900 uppercase tracking-wider">
                    {contextNames[group.name] || group.name}
                  </h3>
                  {#if contextDescriptions[group.name]}
                    <p class="text-xs text-gray-500 mt-0.5">{contextDescriptions[group.name]}</p>
                  {/if}
                </div>
                
                <div class="bg-gray-50 rounded-lg border border-gray-200 overflow-hidden">
                  <table class="w-full text-sm">
                    <tbody class="divide-y divide-gray-200">
                      {#each group.shortcuts as shortcut}
                        <tr class="hover:bg-gray-100 transition-colors">
                          <td class="px-4 py-2.5 w-1/3">
                            <KeyboardShortcut keys={parseKeys(shortcut)} size="sm" />
                          </td>
                          <td class="px-4 py-2.5 text-gray-700">
                            {shortcut.description}
                          </td>
                        </tr>
                      {/each}
                    </tbody>
                  </table>
                </div>
              </section>
            {/each}
          </div>
        {/if}
        
        <!-- Tips -->
        <div class="mt-8 p-4 bg-blue-50 rounded-lg border border-blue-100">
          <h4 class="text-sm font-semibold text-blue-900 mb-2">Pro Tips</h4>
          <ul class="text-sm text-blue-800 space-y-1 list-disc list-inside">
            <li>Press <kbd class="px-1.5 py-0.5 bg-white rounded-sm border border-blue-200 font-mono text-xs">?</kbd> from anywhere to open this help</li>
            <li>Use <kbd class="px-1.5 py-0.5 bg-white rounded-sm border border-blue-200 font-mono text-xs">{getModifierKey()}</kbd> + <kbd class="px-1.5 py-0.5 bg-white rounded-sm border border-blue-200 font-mono text-xs">K</kbd> for quick search</li>
            <li>Navigation shortcuts like <kbd class="px-1.5 py-0.5 bg-white rounded-sm border border-blue-200 font-mono text-xs">G</kbd> then <kbd class="px-1.5 py-0.5 bg-white rounded-sm border border-blue-200 font-mono text-xs">V</kbd> work anywhere</li>
          </ul>
        </div>
      </div>
      
      <!-- Footer -->
      <div class="px-6 py-3 bg-gray-50 border-t border-gray-200 flex items-center justify-between text-xs text-gray-500">
        <span>Press <kbd class="px-1.5 py-0.5 bg-white rounded-sm border border-gray-200 font-mono">?</kbd> anytime to show this dialog</span>
        <span>{filteredGroups.reduce((acc, g) => acc + g.shortcuts.length, 0)} shortcuts</span>
      </div>
    </div>
  </div>
{/if}
