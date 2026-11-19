<script lang="ts">
	import { Server } from 'lucide-svelte';
	import type { ComponentType } from 'svelte';

	interface NavItem {
		id: string;
		label: string;
		icon: ComponentType;
		href: string;
	}

	interface NodeItem {
		id: string;
		name: string;
		status?: string;
	}

	interface Props {
		navItems: NavItem[];
		nodes: NodeItem[];
		isActive: (href: string) => boolean;
		handleNavClick: (href: string) => void;
	}

	let { navItems, nodes, isActive, handleNavClick }: Props = $props();
</script>

<div class="menu-content">
	<!-- Main Navigation -->
	<ul class="nav-list" role="menubar">
		{#each navItems as item}
			<li role="none">
				<a
					href={item.href}
					role="menuitem"
					class="nav-item"
					class:active={isActive(item.href)}
					aria-current={isActive(item.href) ? 'page' : undefined}
					onclick={(e) => {
						e.preventDefault();
						handleNavClick(item.href);
					}}
				>
					<item.icon size={20} aria-hidden="true" />
					<span>{item.label}</span>
				</a>
			</li>
		{/each}
	</ul>

	<!-- Nodes Section -->
	{#if nodes.length > 0}
		<div class="section-divider" role="separator"></div>

		<div class="section-title">Nodes</div>

		<ul class="nav-list" role="menubar">
			{#each nodes as node}
				<li role="none">
					<a
						href={`/nodes/${node.id}`}
						role="menuitem"
						class="nav-item"
						class:active={isActive(`/nodes/${node.id}`)}
						aria-current={isActive(`/nodes/${node.id}`) ? 'page' : undefined}
						onclick={(e) => {
							e.preventDefault();
							handleNavClick(`/nodes/${node.id}`);
						}}
					>
						<Server size={20} aria-hidden="true" />
						<span>{node.name}</span>
						<span class="status-indicator" class:online={node.status === 'online'} aria-label={node.status === 'online' ? 'Online' : 'Offline'}></span>
					</a>
				</li>
			{/each}
		</ul>
	{/if}
</div>

<style>
	.menu-content {
		flex: 1;
		overflow-y: auto;
		padding: 0.5rem 0;
	}

	.nav-list {
		list-style: none;
		margin: 0;
		padding: 0;
	}

	.nav-item {
		display: flex;
		align-items: center;
		gap: 0.75rem;
		padding: 0.75rem 1rem;
		color: var(--chv-color-neutral-300);
		text-decoration: none;
		font-size: 0.875rem;
		transition: all 0.15s ease;
		position: relative;
	}

	.nav-item:hover {
		background: rgba(255, 255, 255, 0.05);
		color: var(--chv-color-neutral-50);
	}

	.nav-item.active {
		background: rgba(var(--color-primary-rgb), 0.16);
		color: #fff7ef;
	}

	.nav-item.active::before {
		content: '';
		position: absolute;
		left: 0;
		top: 50%;
		transform: translateY(-50%);
		width: 3px;
		height: 20px;
		background: var(--color-primary);
		border-radius: 0 2px 2px 0;
	}

	.nav-item:focus-visible {
		outline: 2px solid var(--color-primary);
		outline-offset: -2px;
	}

	.section-divider {
		height: 1px;
		background: rgba(255, 255, 255, 0.08);
		margin: 0.5rem 1rem;
	}

	.section-title {
		padding: 0.5rem 1rem;
		font-size: 0.625rem;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.1em;
		color: var(--chv-color-neutral-400);
	}

	.status-indicator {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: #ef4444;
		margin-left: auto;
	}

	.status-indicator.online {
		background: var(--color-success);
	}
</style>
