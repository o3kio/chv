<script lang="ts">
	import { Database, Menu, X } from 'lucide-svelte';

	interface Props {
		isOpen: boolean;
		toggleMenu: () => void;
	}

	let { isOpen, toggleMenu }: Props = $props();
</script>

<header class="mobile-header">
	<div class="header-content">
		<!-- Logo -->
		<div class="logo">
			<div class="logo-icon">
				<Database size={20} aria-hidden="true" />
			</div>
			<span class="logo-text">CHV</span>
		</div>

		<!-- Menu Button -->
		<button
			type="button"
			class="menu-button"
			onclick={toggleMenu}
			aria-expanded={isOpen}
			aria-controls="mobile-nav-menu"
			aria-label={isOpen ? 'Close menu' : 'Open menu'}
		>
			{#if isOpen}
				<X size={24} aria-hidden="true" />
			{:else}
				<Menu size={24} aria-hidden="true" />
			{/if}
		</button>
	</div>
</header>

<style>
	.mobile-header {
		display: none;
		position: fixed;
		top: 0;
		left: 0;
		right: 0;
		z-index: 50;
		height: 56px;
		background: var(--bg-sidebar);
		border-bottom: 1px solid rgba(255, 255, 255, 0.08);
	}

	.header-content {
		display: flex;
		align-items: center;
		justify-content: space-between;
		height: 100%;
		padding: 0 1rem;
	}

	.logo {
		display: flex;
		align-items: center;
		gap: 0.75rem;
	}

	.logo-icon {
		width: 32px;
		height: 32px;
		border-radius: 8px;
		background: var(--color-primary);
		display: flex;
		align-items: center;
		justify-content: center;
		color: white;
	}

	.logo-text {
		font-size: 1.125rem;
		font-weight: 600;
		color: white;
	}

	.menu-button {
		width: 44px;
		height: 44px;
		display: flex;
		align-items: center;
		justify-content: center;
		border-radius: 8px;
		color: var(--chv-color-neutral-400);
		background: transparent;
		border: none;
		cursor: pointer;
		transition: all 0.15s ease;
	}

	.menu-button:hover {
		background: rgba(255, 255, 255, 0.1);
		color: var(--chv-color-neutral-50);
	}

	.menu-button:focus-visible {
		outline: 2px solid var(--color-primary);
		outline-offset: -2px;
	}

	/* Show mobile header only on small screens */
	@media (max-width: 960px) {
		.mobile-header {
			display: block;
		}
	}
</style>
