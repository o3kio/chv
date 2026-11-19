<script lang="ts">
  import { page } from '$app/state';
</script>

<div class="flex min-h-[60vh] items-center justify-center p-8">
  <div class="text-center max-w-md">
    <h1 class="text-(length:--text-3xl) font-semibold text-neutral-900 mb-2">
      {page.status}
    </h1>
    <p class="text-neutral-600 text-(length:--text-base)">
      {page.error?.message ?? 'An unexpected error occurred.'}
    </p>
    <a href="/" class="inline-block mt-6 px-4 py-2 rounded-lg bg-primary text-white hover:bg-(--color-primary-hover) transition-colors">
      Return home
    </a>
  </div>
</div>
