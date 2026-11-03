<script lang="ts">
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import { browser } from '$app/env';
	import { slide, fade } from 'svelte/transition';
	import {
		BellRing,
		Database,
		LayoutGrid,
		Image as ImageIcon,
		HardDrive,
		Network,
		Settings
	} from 'lucide-svelte';
	import { clearToken } from '#lib/api/client.ts';
	import MobileNavHeader from '#lib/components/shell/MobileNavHeader.svelte';
	import MobileNavList from '#lib/components/shell/MobileNavList.svelte';
	import MobileNavFooter from '#lib/components/shell/MobileNavFooter.svelte';

	// Props
	interface Props {
		nodes?: import('#lib/api/nodes.ts').Node[];
		userName?: string;
		userEmail?: string;
	}

	let {
		nodes = [],
		userName = 'Administrator',
		userEmail = 'admin@chv.local'
	}: Props = $props();

	let isOpen = $state(false);
	let expandedNodes = $state<Set<string>>(new Set(['datacenter']));

	let currentPath = $derived(page.url.pathname);

	const navItems = [
		{ id: 'overview', label: 'Overview', icon: LayoutGrid, href: '/' },
		{ id: 'global-images', label: 'Images', icon: ImageIcon, href: '/images' },
		{ id: 'global-networks', label: 'Networks', icon: Network, href: '/networks' },
		{ id: 'global-storage', label: 'Storage Pools', icon: HardDrive, href: '/volumes' },
		{ id: 'global-alerts', label: 'Alerts', icon: BellRing, href: '/alerts' },
		{ id: 'settings', label: 'Settings', icon: Settings, href: '/settings' },
	];

	function toggleMenu() {
		isOpen = !isOpen;
		// Prevent body scroll when menu is open
		if (browser) {
			document.body.style.overflow = isOpen ? 'hidden' : '';
		}
	}

	function closeMenu() {
		isOpen = false;
		if (browser) {
			document.body.style.overflow = '';
		}
	}

	function handleNavClick(href: string) {
		goto(href);
		closeMenu();
	}

	function toggleNode(nodeId: string) {
		if (expandedNodes.has(nodeId)) {
			expandedNodes.delete(nodeId);
		} else {
			expandedNodes.add(nodeId);
		}
		expandedNodes = expandedNodes;
	}

	function isActive(href: string): boolean {
		if (href === '/') return currentPath === '/';
		return currentPath.startsWith(href);
	}

	async function handleLogout() {
		try {
			const { createAPIClient } = await import('#lib/api/client.ts');
			await createAPIClient().logout();
		} catch {
			// Best-effort: clear the client hints regardless.
		}
		clearToken();
		goto('/login');
		closeMenu();
	}
</script>

<!-- Mobile Header -->
<MobileNavHeader {isOpen} {toggleMenu} />

<!-- Mobile Menu Overlay -->
{#if isOpen}
	<!-- Backdrop -->
	<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
	<div
		class="menu-backdrop"
		transition:fade={{ duration: 200 }}
		onclick={closeMenu}
		aria-hidden="true"
	></div>

	<!-- Menu Panel -->
	<nav
		id="mobile-nav-menu"
		class="menu-panel"
		transition:slide={{ duration: 200, axis: 'x' }}
		aria-label="Mobile navigation"
	>
		<div class="menu-header">
			<div class="logo">
				<div class="logo-icon">
					<Database size={20} aria-hidden="true" />
				</div>
				<div>
					<div class="logo-text">CHV Manager</div>
					<div class="logo-subtitle">Virtualization Platform</div>
				</div>
			</div>
		</div>

		<MobileNavList {navItems} {nodes} {isActive} {handleNavClick} />

		<!-- User Section -->
		<MobileNavFooter {userName} {userEmail} {handleLogout} />
	</nav>
{/if}

<style>
	.menu-backdrop {
		position: fixed;
		inset: 0;
		background: rgba(0, 0, 0, 0.5);
		z-index: 40;
	}

	.menu-panel {
		position: fixed;
		top: 0;
		left: 0;
		bottom: 0;
		width: 280px;
		max-width: 80vw;
		background: var(--bg-sidebar);
		z-index: 41;
		display: flex;
		flex-direction: column;
		border-right: 1px solid rgba(255, 255, 255, 0.08);
	}

	.menu-header {
		padding: 1rem;
		border-bottom: 1px solid rgba(255, 255, 255, 0.08);
		background: rgba(0, 0, 0, 0.14);
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

	.logo-subtitle {
		font-size: 0.625rem;
		color: var(--color-neutral-400);
	}
</style>
