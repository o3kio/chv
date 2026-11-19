<script lang="ts">
  import { getModifierKey } from '#lib/stores/keyboard.svelte.ts';
</script>

<div class="px-4 py-2 bg-gray-50 border-t border-gray-200 flex items-center justify-between text-xs text-gray-500">
  <div class="flex items-center gap-3">
    <span class="flex items-center gap-1">
      <kbd class="px-1.5 py-0.5 bg-white rounded-sm border border-gray-200">↑↓</kbd>
      <span>Navigate</span>
    </span>
    <span class="flex items-center gap-1">
      <kbd class="px-1.5 py-0.5 bg-white rounded-sm border border-gray-200">↵</kbd>
      <span>Select</span>
    </span>
  </div>
  <div class="flex items-center gap-2">
    <span>{getModifierKey()} + Shift + P</span>
    <span>to open</span>
  </div>
</div>
