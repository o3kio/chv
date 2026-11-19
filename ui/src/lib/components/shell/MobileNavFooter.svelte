<script lang="ts">
	import { LogOut } from 'lucide-svelte';

	interface Props {
		userName: string;
		userEmail: string;
		handleLogout: () => void;
	}

	let { userName, userEmail, handleLogout }: Props = $props();
</script>

<div class="menu-footer">
	<div class="user-info">
		<div class="user-avatar">
			{userName.slice(0, 2).toUpperCase()}
		</div>
		<div class="user-details">
			<div class="user-name">{userName}</div>
			<div class="user-email">{userEmail}</div>
		</div>
	</div>

	<button
		type="button"
		class="logout-button"
		onclick={handleLogout}
		aria-label="Sign out"
	>
		<LogOut size={18} aria-hidden="true" />
	</button>
</div>

<style>
	.menu-footer {
		padding: 1rem;
		border-top: 1px solid rgba(255, 255, 255, 0.08);
		background: rgba(0, 0, 0, 0.14);
		display: flex;
		align-items: center;
		justify-content: space-between;
	}

	.user-info {
		display: flex;
		align-items: center;
		gap: 0.75rem;
	}

	.user-avatar {
		width: 36px;
		height: 36px;
		border-radius: 50%;
		background: rgba(255, 255, 255, 0.12);
		display: flex;
		align-items: center;
		justify-content: center;
		font-size: 0.75rem;
		font-weight: 600;
		color: white;
	}

	.user-details {
		min-width: 0;
	}

	.user-name {
		font-size: 0.875rem;
		font-weight: 500;
		color: white;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.user-email {
		font-size: 0.75rem;
		color: var(--chv-color-neutral-400);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.logout-button {
		width: 36px;
		height: 36px;
		border-radius: 8px;
		display: flex;
		align-items: center;
		justify-content: center;
		color: var(--chv-color-neutral-400);
		background: transparent;
		border: none;
		cursor: pointer;
		transition: all 0.15s ease;
	}

	.logout-button:hover {
		background: rgba(var(--color-danger-rgb), 0.12);
		color: var(--color-danger);
	}

	.logout-button:focus-visible {
		outline: 2px solid var(--color-primary);
		outline-offset: -2px;
	}
</style>
