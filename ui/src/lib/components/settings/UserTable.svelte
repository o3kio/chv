<script lang="ts">
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import InventoryTable from '#lib/components/shell/InventoryTable.svelte';
	import StatusBadge from '#lib/components/shell/StatusBadge.svelte';
	import { UserCheck, Pencil, Trash2 } from 'lucide-svelte';
	import type { UserItem } from '#lib/bff/users.ts';
	import type { ShellTone } from '#lib/shell/app-shell.ts';

	interface Props {
		users?: UserItem[];
		loading?: boolean;
		onedit?: (user: UserItem) => void;
		ondelete?: (user: UserItem) => void;
	}

	let {
		users = [],
		loading = false,
		onedit,
		ondelete
	}: Props = $props();

	const columns = [
		{ key: 'username', label: 'Identity ID' },
		{ key: 'display_name', label: 'Alias' },
		{ key: 'role', label: 'RBAC_Role', align: 'center' as const },
		{ key: 'email', label: 'Fabric_Email' },
		{ key: 'last_login_at', label: 'Last_Sync' },
		{ key: 'actions', label: '', align: 'right' as const }
	];

	function roleTone(role: string): ShellTone {
		switch (role) {
			case 'admin': return 'warning';
			case 'operator': return 'healthy';
			default: return 'neutral' as any;
		}
	}
</script>

<SectionCard title="Registered Operator Registry" icon={UserCheck}>
	{#if loading}
		<div class="skeleton-table"></div>
	{:else if users.length === 0}
		<p class="empty-hint">Identity registry is currently empty. Initialize first operator.</p>
	{:else}
		<InventoryTable
			{columns}
			rows={users.map(u => ({
				...u,
				role: { label: u.role.toUpperCase(), tone: roleTone(u.role) }
			}))}
		>
			{#snippet cell({ column, row })}
				{#if column.key === 'username'}
					<span class="identity-id">{row.username}</span>
				{:else if column.key === 'role'}
					<StatusBadge label={row.role.label} tone={row.role.tone} />
				{:else if column.key === 'actions'}
					<div class="action-strip">
						<button type="button" class="btn-icon" onclick={() => onedit?.(row as unknown as UserItem)} title="Edit user" aria-label="Edit user {row.username}">
							<Pencil size={12} />
						</button>
						<button type="button" class="btn-icon danger" onclick={() => ondelete?.(row as unknown as UserItem)} title="Delete user" aria-label="Delete user {row.username}">
							<Trash2 size={12} />
						</button>
					</div>
				{:else}
					<span class="cell-text">{(row as Record<string, unknown>)[column.key] || '—'}</span>
				{/if}
			{/snippet}
		</InventoryTable>
	{/if}
</SectionCard>

<style>
	.identity-id {
		font-weight: 800;
		font-family: var(--chv-font-mono);
		color: var(--chv-color-neutral-900);
	}

	.cell-text {
		font-size: 11px;
		color: var(--chv-color-neutral-600);
	}

	.action-strip {
		display: flex;
		gap: 0.25rem;
		justify-content: flex-end;
	}

	.btn-icon {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 1.75rem;
		height: 1.75rem;
		border-radius: 0.25rem;
		border: 1px solid var(--shell-line);
		background: transparent;
		color: var(--shell-text-secondary);
		cursor: pointer;
		transition: background 0.1s, color 0.1s;
	}

	.btn-icon:hover {
		background: var(--shell-surface-muted);
		color: var(--shell-text);
	}

	.btn-icon.danger:hover {
		background: rgba(var(--color-danger-rgb), 0.1);
		color: var(--color-danger);
		border-color: var(--color-danger);
	}

	.empty-hint {
		font-size: 11px;
		color: var(--chv-color-neutral-400);
		padding: 2rem;
		text-align: center;
	}
</style>
