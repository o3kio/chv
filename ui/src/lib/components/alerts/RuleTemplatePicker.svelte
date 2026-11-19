<script lang="ts">
	import { RULE_TEMPLATES, type RuleTemplate } from '#lib/alerts/rules.ts';

	interface Props {
		selectedId: string | null;
		onSelect: (template: RuleTemplate) => void;
	}

	let { selectedId, onSelect }: Props = $props();

	let selected = $derived(RULE_TEMPLATES.find((t) => t.id === selectedId) ?? null);
</script>

<div class="mb-4">
	<span class="block text-[10px] font-bold uppercase tracking-wider text-(--shell-text-muted) mb-1">
		Start from a template
	</span>
	<div class="grid grid-cols-1 sm:grid-cols-2 gap-2">
		{#each RULE_TEMPLATES as template (template.id)}
			<button
				type="button"
				class="text-left p-2 rounded-xs border bg-(--bg-surface) cursor-pointer transition-colors {selectedId === template.id
					? 'border-primary'
					: 'border-neutral-300 hover:border-neutral-400'}"
				onclick={() => onSelect(template)}
			>
				<span class="block text-xs font-bold text-neutral-900">{template.title}</span>
				<span class="block text-[10px] text-neutral-600 mt-0.5">{template.rule_type}</span>
			</button>
		{/each}
	</div>
	{#if selected}
		<p class="text-[10px] text-(--shell-text-muted) mt-2">{selected.description}</p>
	{/if}
</div>
