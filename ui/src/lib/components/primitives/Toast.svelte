<script lang="ts">
	import { toast, type ToastType } from '#lib/stores/toast.svelte.ts';
	import { CheckCircle2, AlertTriangle, Info as InfoIcon, X } from 'lucide-svelte';

	interface Props {
		id: string;
		type: ToastType;
		message: string;
	}

	let { id, type, message }: Props = $props();

	function handleDismiss() {
		toast.dismiss(id);
	}
</script>

<div
	class="toast toast--{type}"
	role="alert"
	aria-live={type === 'error' ? 'assertive' : 'polite'}
	aria-atomic="true"
>
	<div class="toast-indicator"></div>
	
	<div class="toast-icon">
		{#if type === 'success'}<CheckCircle2 size={16} />
		{:else if type === 'error'}<AlertTriangle size={16} />
		{:else}<InfoIcon size={16} />{/if}
	</div>

	<div class="toast-content">
		{message}
	</div>

	<button
		onclick={handleDismiss}
		class="toast-close"
		aria-label="Dismiss notification"
		type="button"
	>
		<X size={14} />
	</button>
</div>

<style>
	.toast {
		width: 320px;
		max-width: 100%;
		background: var(--bg-surface);
		border: 1px solid var(--border-strong);
		display: flex;
		align-items: center;
		gap: 0.75rem;
		padding: 0.75rem 1rem;
		position: relative;
		box-shadow: 0 4px 20px rgba(0, 0, 0, 0.2);
	}

	.toast-indicator {
		position: absolute;
		left: 0;
		top: 0;
		bottom: 0;
		width: 3px;
		background: var(--chv-color-neutral-300);
	}

	.toast--success .toast-indicator { background: var(--color-success); }
	.toast--error .toast-indicator { background: var(--color-danger); }
	.toast--info .toast-indicator { background: var(--color-info); }

	.toast-icon {
		display: flex;
		flex-shrink: 0;
	}

	.toast--success .toast-icon { color: var(--color-success); }
	.toast--error .toast-icon { color: var(--color-danger); }
	.toast--info .toast-icon { color: var(--color-info); }

	.toast-content {
		flex: 1;
		font-size: 11px;
		font-weight: 600;
		color: var(--chv-color-neutral-800);
		line-height: 1.4;
	}

	.toast-close {
		flex-shrink: 0;
		background: transparent;
		border: none;
		color: var(--chv-color-neutral-400);
		cursor: pointer;
		padding: 2px;
		display: grid;
		place-items: center;
		transition: color 0.1s ease;
	}

	.toast-close:hover {
		color: var(--chv-color-neutral-900);
	}
</style>
