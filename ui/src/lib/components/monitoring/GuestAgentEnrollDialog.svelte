<script lang="ts">
	import Modal from '#lib/components/primitives/Modal.svelte';
	import Button from '#lib/components/primitives/Button.svelte';
	import { KeyRound } from 'lucide-svelte';
	import {
		issueGuestAgentClaim,
		type GuestAgentClaimResponse
	} from '#lib/bff/monitoringAgents.ts';
	import { toast } from '#lib/stores/toast.svelte.ts';

	interface Props {
		open?: boolean;
		vmId: string;
		vmName?: string;
		onClose: () => void;
	}

	let { open = $bindable(false), vmId, vmName, onClose }: Props = $props();

	let issuing = $state(false);
	/** The one-time claim. Held only in component state; dropped with
	 * the dialog and never persisted or logged. */
	let claim = $state<GuestAgentClaimResponse | null>(null);
	let error = $state<string | null>(null);
	let copied = $state(false);

	async function issue() {
		if (issuing) return;
		issuing = true;
		error = null;
		try {
			claim = await issueGuestAgentClaim(vmId);
		} catch (err) {
			error = err instanceof Error ? err.message : 'claim issuance failed';
			claim = null;
		} finally {
			issuing = false;
		}
	}

	async function copyToken() {
		if (!claim) return;
		try {
			await navigator.clipboard.writeText(claim.claim_token);
			copied = true;
			setTimeout(() => (copied = false), 2000);
		} catch {
			toast.error('Clipboard unavailable — copy the token manually');
		}
	}

	function close() {
		claim = null;
		error = null;
		copied = false;
		onClose();
	}

	$effect(() => {
		if (open && !claim && !issuing && !error) {
			issue();
		}
		if (!open) {
			claim = null;
			error = null;
		}
	});
</script>

<Modal bind:open closeOnBackdrop={false} onClose={close}>
	{#snippet header()}
		<div class="flex items-center gap-3">
			<KeyRound class="h-5 w-5 text-(--color-info)" aria-hidden="true" />
			<h2 id="modal-title" class="text-base font-semibold text-(--shell-text)">
				Enroll guest monitoring agent
			</h2>
		</div>
	{/snippet}

	{#if issuing}
		<p class="text-sm text-(--shell-text-secondary)">Issuing a one-time claim…</p>
	{:else if error}
		<p class="text-sm text-danger">{error}</p>
	{:else if claim}
		<div class="space-y-4">
			<p class="text-sm text-(--shell-text-secondary)">
				Install <code class="text-xs">chv-monitor-agent</code> in
				{vmName ?? vmId}, place the claim token at
				<code class="text-xs">/var/lib/chv-monitor/claim</code> (owner
				<code class="text-xs">chv-monitor</code>, mode 0600), and start the service. The agent
				enrolls over mutual TLS on its next run.
			</p>
			<div
				class="bg-(--shell-surface-muted) rounded-lg p-3 border border-(--shell-line)"
			>
				<p
					class="text-xs font-medium text-(--shell-text-muted) uppercase tracking-wider mb-2"
				>
					Claim token — shown once, never again
				</p>
				<p class="font-mono text-xs break-all text-(--shell-text)">
					{claim.claim_token}
				</p>
				{#if claim.server_url}
					<p class="text-xs text-(--shell-text-muted) mt-2">
						Manager: {claim.server_url}
					</p>
				{/if}
			</div>
			<p class="text-xs text-(--shell-text-muted)">
				The token expires
				{new Date(claim.expires_at_ms).toLocaleString()} and is single-use. If it is lost or
				expires unused, simply issue a new one.
			</p>
		</div>
	{/if}

	{#snippet footer()}
		<div class="flex justify-between w-full">
			<Button variant="secondary" size="sm" onclick={copyToken} disabled={!claim}>
				{copied ? 'Copied' : 'Copy token'}
			</Button>
			<Button variant="primary" size="sm" onclick={close}>Done</Button>
		</div>
	{/snippet}
</Modal>
