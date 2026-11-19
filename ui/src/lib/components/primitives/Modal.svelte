<script lang="ts">
	import type { Snippet } from 'svelte';
	import FocusTrap from '../shared/FocusTrap.svelte';

	interface Props {
		open: boolean;
		title?: string;
		width?: 'default' | 'wide';
		closeOnBackdrop?: boolean;
		closeOnEscape?: boolean;
		onClose?: () => void;
		description?: string;
		header?: Snippet;
		children?: Snippet;
		footer?: Snippet;
	}

	let {
		open = $bindable(false),
		title,
		width = 'default',
		closeOnBackdrop = true,
		closeOnEscape = true,
		onClose,
		description,
		header,
		children,
		footer
	}: Props = $props();

	const widthClasses: Record<string, string> = {
		default: 'w-[480px]',
		wide: 'w-[640px]'
	};

	let modalRef = $state<HTMLDivElement | null>(null);
	let closeButtonRef = $state<HTMLButtonElement | null>(null);
	let previouslyFocusedElement = $state<HTMLElement | null>(null);
	let isVisible = $state(false);
	let isClosing = $state(false);
	let modalId = $state(`modal-${Math.random().toString(36).slice(2, 9)}`);
	let titleId = $derived(title ? `${modalId}-title` : undefined);
	let descId = $derived(description ? `${modalId}-description` : undefined);

	function handleKeydown(event: KeyboardEvent) {
		if (!open) return;
		if (event.key === 'Escape' && closeOnEscape) {
			event.preventDefault();
			handleClose();
			return;
		}
	}

	function handleBackdropClick(event: MouseEvent) {
		if (closeOnBackdrop && event.target === event.currentTarget) {
			handleClose();
		}
	}

	function handleClose() {
		if (isClosing) return;
		isClosing = true;
		setTimeout(() => {
			open = false;
			isClosing = false;
			isVisible = false;
			onClose?.();
		}, 200);
	}

	$effect(() => {
		if (open) {
			previouslyFocusedElement = document.activeElement as HTMLElement;
			// Use setTimeout instead of requestAnimationFrame to avoid race conditions
			const focusTimeout = setTimeout(() => {
				isVisible = true;
				if (closeButtonRef && document.contains(closeButtonRef)) {
					closeButtonRef.focus();
				}
			}, 10);
			document.body.style.overflow = 'hidden';
			return () => {
				clearTimeout(focusTimeout);
			};
		} else {
			document.body.style.overflow = '';
			if (previouslyFocusedElement && !isClosing && document.contains(previouslyFocusedElement)) {
				previouslyFocusedElement.focus();
			}
		}
		return () => {
			document.body.style.overflow = '';
		};
	});
</script>

<svelte:window onkeydown={handleKeydown} />

{#if open}
	<!-- Backdrop -->
	<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
	<div
		class="fixed inset-0 z-50 flex items-center justify-center bg-black/50 transition-opacity duration-200 ease-out"
		class:opacity-0={!isVisible || isClosing}
		class:opacity-100={isVisible && !isClosing}
		onclick={handleBackdropClick}
		role="presentation"
	>
		<!-- Modal Container with Focus Trap -->
		<FocusTrap active={isVisible && !isClosing} onEscape={closeOnEscape ? handleClose : undefined}>
			<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
			<div
				bind:this={modalRef}
				role="dialog"
				aria-modal="true"
				aria-labelledby={titleId}
				aria-describedby={descId}
				tabindex="-1"
				class="{widthClasses[width]} max-h-[80vh] overflow-hidden rounded-lg bg-(--bg-surface) shadow-[0_4px_16px_rgba(0,0,0,0.15)] transition-all duration-200 ease-out mx-4"
				class:scale-95={!isVisible || isClosing}
				class:scale-100={isVisible && !isClosing}
				class:opacity-0={!isVisible || isClosing}
				class:opacity-100={isVisible && !isClosing}
				onclick={(e) => e.stopPropagation()}
			>
				<div class="flex h-14 items-center justify-between border-b border-line px-6">
					{#if header}
						{@render header()}
					{:else if title}
						<div>
							<h2 id={titleId} class="text-base font-semibold text-ink">{title}</h2>
							{#if description}
								<p id={descId} class="text-sm text-muted mt-0.5">{description}</p>
							{/if}
						</div>
					{:else}
						<div></div>
					{/if}
					<button
						bind:this={closeButtonRef}
						onclick={handleClose}
						class="flex h-8 w-8 shrink-0 items-center justify-center rounded-sm text-muted transition-colors hover:bg-black/5 hover:text-ink focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2"
						aria-label="Close modal"
						type="button"
					>
						<svg xmlns="http://www.w3.org/2000/svg" width={20} height={20} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
							<path d="M18 6 6 18" />
							<path d="m6 6 12 12" />
						</svg>
					</button>
				</div>
				{#if children}
					<div class="max-h-[calc(80vh-3.5rem-4.5rem)] overflow-y-auto p-6">
						{@render children()}
					</div>
				{/if}
				{#if footer}
					<div class="flex justify-end gap-2 border-t border-line px-6 py-4">
						{@render footer()}
					</div>
				{/if}
			</div>
		</FocusTrap>
	</div>
{/if}
