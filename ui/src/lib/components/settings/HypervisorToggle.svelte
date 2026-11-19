<script lang="ts">
	interface Props {
		checked: boolean;
		label: string;
		description: string;
		onchange: (checked: boolean) => void;
	}

	let { checked, label, description, onchange }: Props = $props();
</script>

<label class="toggle-control">
	<input type="checkbox" {checked} onchange={(e) => onchange(e.currentTarget.checked)} />
	<div class="toggle-meta">
		<span class="label">{label}</span>
		<span class="desc">{description}</span>
	</div>
</label>

<style>
	.toggle-control {
		display: flex;
		gap: 0.75rem;
		cursor: pointer;
	}

	.toggle-control input {
		margin-top: 0.125rem;
	}

	.toggle-meta {
		display: flex;
		flex-direction: column;
		gap: 0.125rem;
	}

	.toggle-meta .label {
		font-size: 11px;
		font-weight: 800;
		color: var(--chv-color-neutral-900);
	}

	.toggle-meta .desc {
		font-size: 10px;
		color: var(--chv-color-neutral-500);
		line-height: 1.4;
	}
</style>
