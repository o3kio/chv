<script lang="ts">
	interface Props {
		step?: number;
		submitting?: boolean;
		canProceedToStep2?: boolean | (() => boolean);
		onCancel?: () => void;
		onNext?: () => void;
		onBack?: () => void;
		onSubmit?: () => void;
	}

	let {
		step = 1,
		submitting = false,
		canProceedToStep2 = false,
		onCancel = () => {},
		onNext = () => {},
		onBack = () => {},
		onSubmit = () => {}
	}: Props = $props();

	const canProceed = $derived(typeof canProceedToStep2 === 'function' ? canProceedToStep2() : canProceedToStep2);
</script>

{#if step === 1}
	<button
		type="button"
		onclick={onCancel}
		disabled={submitting}
		class="px-4 py-2 rounded-sm border border-line text-ink bg-white hover:bg-chrome transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
	>
		Cancel
	</button>
	<button
		type="button"
		onclick={onNext}
		disabled={!canProceed || submitting}
		class="px-4 py-2 rounded-sm bg-primary text-white font-medium hover:bg-primary/90 transition-colors disabled:bg-primary/30 disabled:cursor-not-allowed"
	>
		Next
	</button>
{:else if step === 2}
	<button
		type="button"
		onclick={onBack}
		disabled={submitting}
		class="px-4 py-2 rounded-sm border border-line text-ink bg-white hover:bg-chrome transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
	>
		Back
	</button>
	<button
		type="button"
		onclick={onNext}
		disabled={submitting}
		class="px-4 py-2 rounded-sm bg-primary text-white font-medium hover:bg-primary/90 transition-colors disabled:bg-primary/30 disabled:cursor-not-allowed"
	>
		Next
	</button>
{:else}
	<button
		type="button"
		onclick={onBack}
		disabled={submitting}
		class="px-4 py-2 rounded-sm border border-line text-ink bg-white hover:bg-chrome transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
	>
		Back
	</button>
	<button
		type="button"
		onclick={onSubmit}
		disabled={submitting}
		class="px-4 py-2 rounded-sm bg-primary text-white font-medium hover:bg-primary/90 transition-colors disabled:bg-primary/30 disabled:cursor-not-allowed flex items-center gap-2"
	>
		{#if submitting}
			<svg
				class="animate-spin h-4 w-4"
				xmlns="http://www.w3.org/2000/svg"
				fill="none"
				viewBox="0 0 24 24"
				aria-hidden="true"
			>
				<circle
					class="opacity-25"
					cx="12"
					cy="12"
					r="10"
					stroke="currentColor"
					stroke-width="4"
				></circle>
				<path
					class="opacity-75"
					fill="currentColor"
					d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"
				></path>
			</svg>
		{/if}
		{submitting ? 'Creating...' : 'Create VM'}
	</button>
{/if}
