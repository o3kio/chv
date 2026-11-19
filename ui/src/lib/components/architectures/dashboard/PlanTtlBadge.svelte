<script lang="ts">
	import { onDestroy } from 'svelte';

	interface Props {
		/** ISO 8601 string from `PlanResult.expires_at`. */
		expiresAt: string;
	}

	let { expiresAt }: Props = $props();

	// Live-updating "now" used for the TTL countdown. `onDestroy` clears the
	// interval so the badge doesn't leak handles when the panel unmounts.
	let now = $state(Date.now());
	const interval = setInterval(() => {
		now = Date.now();
	}, 1000);
	onDestroy(() => clearInterval(interval));

	const expiresAtMs = $derived(Date.parse(expiresAt));
	const expired = $derived(Number.isFinite(expiresAtMs) && now > expiresAtMs);
	const remainingMs = $derived(expiresAtMs - now);

	function formatCountdown(ms: number): string {
		if (!Number.isFinite(ms) || ms <= 0) return '0:00';
		const totalSec = Math.max(0, Math.floor(ms / 1000));
		const min = Math.floor(totalSec / 60);
		const sec = totalSec % 60;
		return `${min}:${sec.toString().padStart(2, '0')}`;
	}
</script>

{#if expired}
	<span class="ttl ttl-exp" role="status" data-testid="plan-ttl-expired">
		expired — regenerate
	</span>
{:else}
	<span class="ttl" aria-live="polite" data-testid="plan-ttl-countdown">
		expires in {formatCountdown(remainingMs)}
	</span>
{/if}

<style>
	.ttl {
		font-size: 12px;
		color: var(--chv-color-neutral-600);
		padding: 0.1rem 0.5rem;
		background: var(--chv-color-neutral-50, #f8fafc);
		border: 1px solid var(--chv-color-neutral-200);
		border-radius: var(--radius-xs);
	}
	.ttl-exp {
		color: rgb(153, 27, 27);
		background: rgba(220, 38, 38, 0.08);
		border-color: rgba(220, 38, 38, 0.4);
		font-weight: 600;
	}
</style>
