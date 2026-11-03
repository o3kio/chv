<script lang="ts">
	import { page } from '$app/state';
	import {
		Image,
		Network,
		HardDrive,
		Activity,
		AlertCircle,
		BellRing,
		Settings
	} from 'lucide-svelte';

	function isActive(href: string, pathname: string): boolean {
		if (href === '/') return pathname === '/';
		return pathname === href || pathname.startsWith(`${href}/`);
	}

	const links = [
		{ href: '/images', icon: Image, label: 'Images' },
		{ href: '/networks', icon: Network, label: 'Networks' },
		{ href: '/volumes', icon: HardDrive, label: 'Storage Pools' },
		{ href: '/tasks', icon: Activity, label: 'Tasks' },
		{ href: '/events', icon: AlertCircle, label: 'Events' },
		{ href: '/alerts', icon: BellRing, label: 'Alerts' },
		{ href: '/settings', icon: Settings, label: 'Settings' }
	];
</script>

<div class="flex flex-col gap-1">
	<div class="text-[10px] font-bold uppercase text-[var(--color-neutral-500)] mb-1 pl-2 tracking-wider">Global</div>

	{#each links as link}
		<a
			href={link.href}
			class="flex items-center gap-[0.625rem] py-[0.35rem] px-2 text-[length:var(--text-sm)] text-[var(--color-neutral-300)] no-underline rounded-[var(--radius-xs)] transition-all duration-[120ms] ease-in-out hover:bg-[var(--color-neutral-800)] hover:text-[var(--color-sidebar-text-active,#ffffff)] {isActive(link.href, page.url.pathname) ? 'bg-[var(--color-primary)] text-[var(--color-sidebar-text-active,#ffffff)]' : ''}"
			aria-current={isActive(link.href, page.url.pathname) ? 'page' : undefined}
		>
			<link.icon size={14} />
			<span>{link.label}</span>
		</a>
	{/each}
</div>
