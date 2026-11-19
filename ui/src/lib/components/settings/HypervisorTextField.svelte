<script lang="ts">
	interface Props {
		label: string;
		value: string;
		onchange: (v: string) => void;
	}
	let { label, value, onchange }: Props = $props();
</script>

<div class="field-control">
	<span class="field-label">{label}</span>
	<input
		type="text"
		class="field-input"
		{value}
		onchange={(e) => onchange(e.currentTarget.value)}
	/>
</div>

<style>
	.field-control {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
	}
	.field-label {
		font-size: 10px;
		font-weight: 800;
		color: var(--chv-color-neutral-500);
	}
	.field-input {
		background: var(--bg-surface-muted);
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-xs);
		padding: 0.35rem 0.5rem;
		font-size: 11px;
		font-family: var(--chv-font-mono);
		color: var(--chv-color-neutral-900);
	}
	.field-input:focus {
		outline: none;
		border-color: var(--color-primary);
	}
</style>
