<script lang="ts">
	interface Property {
		label: string;
		value: string | number;
		subtext?: string;
		tone?: 'healthy' | 'warning' | 'degraded' | 'failed' | 'neutral';
	}

	interface Props {
		title?: string;
		properties: Property[];
		columns?: number;
	}

	let { title, properties, columns = 2 }: Props = $props();

	const toneClasses: Record<string, string> = {
		healthy: 'text-success',
		warning: 'text-(--color-warning-dark)',
		degraded: 'text-danger',
		failed: 'text-danger',
		neutral: ''
	};
</script>

<section class="flex flex-col gap-3">
	{#if title}<h3 class="text-(length:--chv-text-xs) font-bold uppercase tracking-wider text-(--shell-text-muted) border-b border-(--shell-line) pb-1 m-0">{title}</h3>{/if}
	
	<div class="grid gap-4 max-[600px]:grid-cols-1!" style="grid-template-columns: repeat({columns}, 1fr);">
		{#each properties as prop}
			<div class="flex flex-col gap-[0.15rem]">
				<span class="text-(length:--chv-text-xs) text-(--shell-text-muted) font-medium">{prop.label}</span>
				<div class="flex flex-col">
					<span class="text-(length:--chv-text-sm) font-semibold text-(--shell-text) {toneClasses[prop.tone ?? 'neutral']}">
						{prop.value}
					</span>
					{#if prop.subtext}
						<span class="text-(length:--chv-text-xs) text-(--shell-text-muted)">{prop.subtext}</span>
					{/if}
				</div>
			</div>
		{/each}
	</div>
</section>
