<script lang="ts">
	import { onDestroy, onMount } from 'svelte';
	import { ShieldCheck } from 'lucide-svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import Button from '#lib/components/primitives/Button.svelte';
	import {
		listGuestAgents,
		revokeGuestAgent,
		forceGuestAgentRotation,
		resetGuestAgentConflict,
		type GuestAgentInventoryItem
	} from '#lib/bff/monitoringAgents.ts';
	import { BFFError } from '#lib/bff/client.ts';
	import { mutateWithRefresh } from '#lib/stores/mutation.svelte.ts';
	import {
		guestAgentActions,
		guestAgentCredentialRemaining,
		guestAgentLastSeen,
		guestAgentStateView,
		type GuestAgentAction
	} from '#lib/monitoring/guestAgent.ts';

	interface Props {
		vmId: string;
		vmName?: string;
		onEnroll?: () => void;
	}

	let { vmId, vmName, onEnroll }: Props = $props();

	let agent = $state<GuestAgentInventoryItem | null>(null);
	let loading = $state(true);
	/** 'disabled' = guest ingestion is not configured on this manager. */
	let disabled = $state(false);
	let error = $state<string | null>(null);
	let busy = $state(false);
	let pollInterval: number | null = null;

	async function load() {
		loading = agent === null && !disabled;
		try {
			const response = await listGuestAgents(vmId);
			if (!response || !Array.isArray(response.agents)) {
				throw new Error('unexpected response shape');
			}
			// Revoked predecessors stay in the registry; the live agent
			// (if any) is the one an operator can still act on. Showing
			// the newest non-revoked row keeps the card actionable.
			agent =
				response.agents.find((a) => a.state !== 'revoked') ?? response.agents[0] ?? null;
			disabled = false;
			error = null;
		} catch (err) {
			if (err instanceof BFFError && err.code === 'guest_ingestion_disabled') {
				disabled = true;
				error = null;
			} else {
				disabled = false;
				error = err instanceof Error ? err.message : 'agent status unavailable';
			}
		} finally {
			loading = false;
		}
	}

	onMount(() => {
		load();
		pollInterval = window.setInterval(load, 30_000);
	});

	onDestroy(() => {
		if (pollInterval) clearInterval(pollInterval);
	});

	let stateView = $derived(
		agent ? guestAgentStateView(agent.state) : guestAgentStateView('unenrolled')
	);
	let actions = $derived(
		agent ? guestAgentActions(agent.state) : (['enroll'] as GuestAgentAction[])
	);

	async function run(action: 'rotate' | 'revoke' | 'reset') {
		const current = agent;
		if (!current || busy) return;
		// The synthesized `enrolling` entry carries no agent identity;
		// its only action is (re-)issuing a claim, which never routes here.
		// Capture into a const: property narrowing does not survive the
		// mutateWithRefresh closure below.
		const agentId = current.agent_id;
		if (!agentId) return;
		busy = true;
		try {
			await mutateWithRefresh(
				async () => {
					if (action === 'rotate') {
						return forceGuestAgentRotation(agentId);
					}
					if (action === 'reset') {
						return resetGuestAgentConflict(agentId);
					}
					return revokeGuestAgent(agentId);
				},
				{
					skipRefresh: true,
					successMessage:
						action === 'rotate'
							? 'Rotation requested; the agent rotates on its next ingest'
							: action === 'reset'
								? 'Identity conflict cleared'
								: 'Agent revoked'
				}
			);
			await load();
		} catch {
			// Error already toasted by mutateWithRefresh.
		} finally {
			busy = false;
		}
	}
</script>

<SectionCard title="Guest monitoring agent">
	{#snippet icon()}
		<ShieldCheck class="h-4 w-4" aria-hidden="true" />
	{/snippet}

	{#if loading}
		<p class="text-sm text-(--shell-text-muted)">Loading agent status…</p>
	{:else if disabled}
		<p class="text-sm text-(--shell-text-secondary)">
			Guest ingestion is not enabled on this manager. VMs are fully usable without it — enabling it
			is a deployment decision (see ADR-026).
		</p>
	{:else if error}
		<p class="text-sm text-danger">{error}</p>
	{:else if !agent || agent.state === 'revoked'}
		<div class="flex items-start justify-between gap-4">
			<div>
				<p class="text-sm text-(--shell-text)">{stateView.label}</p>
				<p class="text-sm text-(--shell-text-secondary)">{stateView.description}</p>
			</div>
			{#if onEnroll && actions.includes('enroll')}
				<Button variant="primary" size="sm" onclick={onEnroll}>Enroll agent</Button>
			{/if}
		</div>
	{:else if agent.state === 'enrolling'}
		<div class="flex items-start justify-between gap-4">
			<div>
				<p class="text-sm text-(--shell-text)">{stateView.label}</p>
				<p class="text-sm text-(--shell-text-secondary)">{stateView.description}</p>
				<p class="text-xs text-(--shell-text-muted) mt-1">
					Claim expires in
					{agent.claim_expires_at_ms ? guestAgentCredentialRemaining(agent.claim_expires_at_ms) : '—'}
					{#if agent.claim_issued_by}· issued by {agent.claim_issued_by}{/if}
				</p>
			</div>
			{#if onEnroll && actions.includes('enroll')}
				<Button variant="secondary" size="sm" onclick={onEnroll}>Re-issue claim</Button>
			{/if}
		</div>
	{:else}
		<div class="flex items-start justify-between gap-4">
			<div class="space-y-1 min-w-0">
				<p class="text-sm font-medium text-(--shell-text)">
					{stateView.label}
					{#if agent.os.name}
						<span class="font-normal text-(--shell-text-secondary)">
							· {agent.os.name}{agent.os.version ? ` ${agent.os.version}` : ''}
						</span>
					{/if}
				</p>
				<p class="text-xs text-(--shell-text-secondary)">{stateView.description}</p>
				<dl class="grid grid-cols-2 gap-x-6 gap-y-1 text-xs text-(--shell-text-muted) mt-2">
					<div>
						<dt class="inline">Last report: </dt>
						<dd class="inline text-(--shell-text-secondary)">
							{guestAgentLastSeen(agent.last_seen_age_seconds, agent.state)}
						</dd>
					</div>
					<div>
						<dt class="inline">Credential: </dt>
						<dd class="inline text-(--shell-text-secondary)">
							epoch {agent.credential_epoch ?? '—'}, expires in
							{agent.credential_expires_at_ms
								? guestAgentCredentialRemaining(agent.credential_expires_at_ms)
								: '—'}
						</dd>
					</div>
					<div>
						<dt class="inline">OS kernel: </dt>
						<dd class="inline text-(--shell-text-secondary)">
							{agent.os.kernel_release ?? 'unknown'}
						</dd>
					</div>
					<div>
						<dt class="inline">VM: </dt>
						<dd class="inline text-(--shell-text-secondary)">{vmName ?? vmId}</dd>
					</div>
				</dl>
			</div>
			<div class="flex flex-col gap-2 shrink-0">
				{#if actions.includes('rotate')}
					<Button variant="secondary" size="sm" disabled={busy} onclick={() => run('rotate')}>
						Force rotation
					</Button>
				{/if}
				{#if actions.includes('reset')}
					<Button variant="secondary" size="sm" disabled={busy} onclick={() => run('reset')}>
						Clear conflict
					</Button>
				{/if}
				{#if actions.includes('revoke')}
					<Button variant="danger" size="sm" disabled={busy} onclick={() => run('revoke')}>
						Revoke
					</Button>
				{/if}
			</div>
		</div>
	{/if}
</SectionCard>
