<script lang="ts">
	import { Send } from 'lucide-svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import Badge from '#lib/components/primitives/Badge.svelte';
	import type { AlertDelivery } from '#lib/bff/alerting.ts';
	import { formatAlertTimestamp } from '#lib/alerts/rules.ts';

	interface Props {
		deliveries: AlertDelivery[];
	}

	let { deliveries }: Props = $props();

	const statusVariant: Record<string, 'default' | 'success' | 'warning' | 'danger'> = {
		delivered: 'success',
		pending: 'warning',
		dead: 'danger'
	};
</script>

<SectionCard title="Recent deliveries" icon={Send}>
	<p class="text-[10px] text-(--shell-text-muted) mb-2">
		Notification outbox audit — webhook and Slack deliveries for firing, resolved and
		acknowledged events.
	</p>
	{#if deliveries.length === 0}
		<p class="text-sm text-(--shell-text-secondary)">
			No notification events yet. Deliveries appear once a rule fires and a destination is
			configured.
		</p>
	{:else}
		<div class="overflow-x-auto">
			<table class="w-full text-xs border-collapse">
				<thead>
					<tr class="text-left text-(--shell-text-muted) border-b border-(--shell-line)">
						<th class="py-1.5 pr-3 font-semibold">Event</th>
						<th class="py-1.5 pr-3 font-semibold">Channel</th>
						<th class="py-1.5 pr-3 font-semibold">Status</th>
						<th class="py-1.5 pr-3 font-semibold">Summary</th>
						<th class="py-1.5 font-semibold">When</th>
					</tr>
				</thead>
				<tbody>
					{#each deliveries as delivery (delivery.event_id)}
						<tr class="border-b border-(--shell-line) last:border-b-0">
							<td class="py-1.5 pr-3 whitespace-nowrap">
								<span class="font-medium text-(--shell-text)">{delivery.event_type}</span>
								{#if delivery.attempts > 1}
									<span class="text-(--shell-text-muted)"> · {delivery.attempts} attempts</span>
								{/if}
							</td>
							<td class="py-1.5 pr-3 text-(--shell-text-secondary) whitespace-nowrap">
								{delivery.channel}
							</td>
							<td class="py-1.5 pr-3 whitespace-nowrap">
								<Badge variant={statusVariant[delivery.status] ?? 'default'} dot>
									{delivery.status}
								</Badge>
								{#if delivery.last_response}
									<span class="block text-[10px] text-(--shell-text-muted) mt-0.5">
										{delivery.last_response}
									</span>
								{/if}
							</td>
							<!-- Summaries are operator-configured or redacted server-side;
							     still plain text interpolation only, never {@html}. -->
							<td class="py-1.5 pr-3 text-(--shell-text-secondary)">{delivery.summary}</td>
							<td class="py-1.5 text-(--shell-text-secondary) whitespace-nowrap">
								{formatAlertTimestamp(delivery.occurred_at_ms)}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</SectionCard>
