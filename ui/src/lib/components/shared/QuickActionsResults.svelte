<script lang="ts">
  import type { QuickAction } from './quick-actions-data';

  interface Props {
    flatActions: QuickAction[];
    groupedActions: Map<string, QuickAction[]>;
    selectedIndex: number;
    query: string;
    selectAction: (action: QuickAction) => void;
    getGlobalIndex: (sectionIndex: number, actionIndex: number) => number;
  }

  let {
    flatActions,
    groupedActions,
    selectedIndex,
    query,
    selectAction,
    getGlobalIndex
  }: Props = $props();
</script>

{#if flatActions.length === 0}
  <div class="px-4 py-8 text-center text-gray-500">
    <p>No actions found for "{query}"</p>
    <p class="text-sm mt-1 text-gray-400">Try a different search term</p>
  </div>
{:else}
  {@const sections = Array.from(groupedActions.entries())}
  {#each sections as [section, actions], sectionIndex}
    <div class="py-1">
      <div class="px-4 py-1.5 text-xs font-semibold text-gray-500 uppercase tracking-wider bg-gray-50">
        {section}
      </div>
      {#each actions as action, actionIndex}
        {@const globalIndex = getGlobalIndex(sectionIndex, actionIndex)}
        {@const Icon = action.icon}
        <button
          type="button"
          data-index={globalIndex}
          role="option"
          aria-selected={selectedIndex === globalIndex}
          class="w-full px-4 py-2.5 flex items-center gap-3 text-left hover:bg-gray-100 transition-colors"
          class:bg-blue-50={selectedIndex === globalIndex}
          onclick={() => selectAction(action)}
        >
          <Icon size={18} class="text-gray-500 shrink-0" />
          <div class="flex-1 min-w-0">
            <div class="text-sm font-medium text-gray-900">{action.title}</div>
            <div class="text-xs text-gray-500">{action.description}</div>
          </div>
          {#if action.shortcut}
            <div class="hidden sm:flex items-center gap-1">
              {#each action.shortcut as key}
                <kbd class="px-1.5 py-0.5 text-xs font-mono bg-gray-100 text-gray-600 rounded-sm border border-gray-200">
                  {key.toUpperCase()}
                </kbd>
              {/each}
            </div>
          {/if}
        </button>
      {/each}
    </div>
  {/each}
{/if}
