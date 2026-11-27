<script lang="ts">
	import Button from '#lib/components/primitives/Button.svelte';
	import { Users, Plus } from 'lucide-svelte';
	import PageHeaderWithAction from '#lib/components/shell/PageHeaderWithAction.svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import CompactMetricCard from '#lib/components/shared/CompactMetricCard.svelte';
	import ErrorState from '#lib/components/shell/ErrorState.svelte';
	import ConfirmAction from '#lib/components/shared/ConfirmAction.svelte';
	import { toast } from '#lib/stores/toast.svelte.ts';
	import { mutateWithRefresh } from '#lib/stores/mutation.svelte.ts';
	import { getStoredToken } from '#lib/api/client.ts';
	import { listUsers, createUser, updateUser, deleteUser, type UserItem } from '#lib/bff/users.ts';
	import type { ShellTone } from '#lib/shell/app-shell.ts';
	import type { PageData } from './$types';
	import UserTable from '#lib/components/settings/UserTable.svelte';
	import UserEditModal from '#lib/components/settings/UserEditModal.svelte';
	import UserSupportPanel from '#lib/components/settings/UserSupportPanel.svelte';

	let { data }: { data: PageData } = $props();

	const token = getStoredToken() ?? undefined;

	let users: UserItem[] = $state([]);
	let loading = $state(false);
	let error = $state(false);

	$effect(() => {
		users = data.model?.users ?? [];
		error = data.model?.state === 'error';
	});

	let createOpen = $state(false);
	let editOpen = $state(false);
	let deleteOpen = $state(false);
	let selectedUser = $state<UserItem | null>(null);

	let createForm = $state({
		username: '', password: '', role: 'viewer', display_name: '', email: '',
		submitting: false, error: ''
	});

	let editForm = $state({
		role: '', display_name: '', email: '', password: '',
		submitting: false, error: ''
	});

	const pageDef = {
		title: 'Access Control Matrix',
		eyebrow: 'SET_IDENTITY_REGISTRY',
		description: 'Manage operator identities, RBAC assignments, and control plane access.',
		icon: Users,
		badges: [
			{ label: 'ADMIN_ONLY', tone: 'warning' as ShellTone },
			{ label: 'AUDITABLE', tone: 'healthy' as ShellTone }
		]
	};

	async function reloadUsers() {
		loading = true;
		error = false;
		try {
			const res = await listUsers(token);
			users = res.items ?? [];
		} catch (err: any) {
			error = true;
			toast.error('Identity registry sync failed');
		} finally {
			loading = false;
		}
	}

	function openCreate() {
		createForm = { username: '', password: '', role: 'viewer', display_name: '', email: '', submitting: false, error: '' };
		createOpen = true;
	}

	function openEdit(user: UserItem) {
		selectedUser = user;
		editForm = { role: user.role, display_name: user.display_name ?? '', email: user.email ?? '', password: '', submitting: false, error: '' };
		editOpen = true;
	}

	async function handleCreateSave(payload: { username?: string; password: string; role: string; display_name: string; email: string }) {
		if (!payload.username?.trim()) { createForm.error = 'IDENTITY_ID_REQUIRED'; return; }
		if (payload.password.length < 8) { createForm.error = 'PWD_MIN_LENGTH_ERR'; return; }
		const username = payload.username;
		createForm.submitting = true;
		try {
			await mutateWithRefresh(
				() => createUser({
					username: username.trim(), password: payload.password,
					role: payload.role, display_name: payload.display_name.trim() || undefined,
					email: payload.email.trim() || undefined
				}, token),
				{
					successMessage: 'Identity created',
					errorMessage: 'Identity creation failed',
					skipRefresh: true,
				}
			);
			createOpen = false;
			await reloadUsers();
		} catch (err: any) {
			createForm.error = err.message || 'Identity creation failed';
		} finally { createForm.submitting = false; }
	}

	async function handleEditSave(payload: { password: string; role: string; display_name: string; email: string }) {
		if (!selectedUser) return;
		const user = selectedUser;
		editForm.submitting = true;
		try {
			await mutateWithRefresh(
				() => updateUser({
					user_id: user.user_id, role: payload.role || undefined,
					display_name: payload.display_name.trim() || undefined,
					email: payload.email.trim() || undefined,
					password: payload.password.trim() || undefined
				}, token),
				{
					successMessage: 'Identity updated',
					errorMessage: 'Identity update failed',
					skipRefresh: true,
				}
			);
			editOpen = false;
			await reloadUsers();
		} catch (err: any) {
			editForm.error = err.message || 'Identity update failed';
		} finally { editForm.submitting = false; }
	}

	async function handleDelete() {
		if (!selectedUser) return;
		const user = selectedUser;
		try {
			await mutateWithRefresh(
				() => deleteUser(user.user_id, token),
				{
					successMessage: 'Identity purged',
					errorMessage: 'Identity purge failed',
					skipRefresh: true,
				}
			);
			await reloadUsers();
		} catch (err: any) {
			// Error already toasted by mutateWithRefresh
		}
	}

	const adminCount = $derived(users.filter(u => u.role === 'admin').length);
</script>

<div class="inventory-page">
	<PageHeaderWithAction page={pageDef as any}>
		{#snippet actions()}
			<div class="header-actions">
				<Button variant="primary" onclick={openCreate}>
					<Plus size={14} />
					ADD_IDENTITY
				</Button>
			</div>
		{/snippet}
	</PageHeaderWithAction>

	{#if error}
		<ErrorState title="Identity registry unreachable" description="Failed to load operator records from the control plane." />
	{:else}
		<div class="inventory-metrics">
			<CompactMetricCard label="Total Identities" value={users.length} color="primary" />
			<CompactMetricCard label="Admin Principals" value={adminCount} color="warning" />
		</div>

		<main class="inventory-main">
			<div class="settings-content">
				<UserTable {users} {loading} onedit={openEdit} ondelete={(user) => { selectedUser = user; deleteOpen = true; }} />
			</div>

			<UserSupportPanel />
		</main>
	{/if}
</div>

<UserEditModal
	user={null}
	bind:open={createOpen}
	submitting={createForm.submitting}
	error={createForm.error}
	onsave={handleCreateSave}
	onclose={() => createOpen = false}
/>

<UserEditModal
	user={selectedUser}
	bind:open={editOpen}
	submitting={editForm.submitting}
	error={editForm.error}
	onsave={handleEditSave}
	onclose={() => editOpen = false}
/>

<ConfirmAction
	bind:open={deleteOpen}
	title="PURGE_IDENTITY"
	description={selectedUser ? `Permanent removal of principal "${selectedUser.username}". Continue?` : ''}
	actionType="delete"
	confirmText="PURGE_PRINCIPAL"
	onConfirm={handleDelete}
/>

<ConfirmAction
	bind:open={deleteOpen}
	title="Delete User"
	description={selectedUser ? `Delete user "${selectedUser.username}"? This action cannot be undone.` : ''}
	actionType="delete"
	confirmText="Delete User"
	onConfirm={handleDelete}
/>

<style>
	.inventory-page {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}

	.header-actions {
		display: flex;
		align-items: center;
	}

	.inventory-metrics {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
		gap: 0.75rem;
	}

	.inventory-main {
		display: grid;
		grid-template-columns: 1fr 300px;
		gap: 1rem;
		align-items: start;
	}

	.settings-content {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

</style>
