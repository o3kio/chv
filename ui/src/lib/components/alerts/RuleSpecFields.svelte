<script lang="ts">
	import type {
		CheckStatusMatch,
		RuleType,
		ThresholdOperator
	} from '#lib/bff/alerting.ts';

	/**
	 * The typed-spec form fields, conditional on the rule type. All
	 * fields are two-way bound back into the editor dialog's state.
	 */
	interface Props {
		ruleType: Exclude<RuleType, 'group'> | 'group';
		/** Rendered summary for group rules (round-tripped unchanged). */
		groupSummary?: string;
		/** Honest hint for editing multi-key dimension matches. */
		dimensionNote?: string;
		/** Marks the dialog's dimension fields as edited. */
		onDimensionEdit?: () => void;
		metricId?: string;
		operator?: ThresholdOperator;
		threshold?: number;
		thresholdPerSecond?: number;
		windowSeconds?: number;
		checkId?: string;
		statusMatch?: CheckStatusMatch;
		dimensionKey?: string;
		dimensionValue?: string;
	}

	let {
		ruleType,
		groupSummary = '',
		dimensionNote = '',
		onDimensionEdit,
		metricId = $bindable(''),
		operator = $bindable('greater_than'),
		threshold = $bindable(0),
		thresholdPerSecond = $bindable(0),
		windowSeconds = $bindable(300),
		checkId = $bindable(''),
		statusMatch = $bindable('critical'),
		dimensionKey = $bindable(''),
		dimensionValue = $bindable('')
	}: Props = $props();

	const inputClass =
		'w-full px-2.5 py-1.5 text-sm border border-(--chv-color-neutral-300) rounded-xs bg-(--bg-surface) text-(--chv-color-neutral-900)';
	const labelClass =
		'block text-[10px] font-bold uppercase tracking-wider text-(--shell-text-muted) mb-1';
</script>

{#if ruleType === 'threshold' || ruleType === 'rate' || ruleType === 'availability'}
	<label class="block">
		<span class={labelClass}>Metric id</span>
		<input class={inputClass} type="text" bind:value={metricId} />
	</label>
	{#if ruleType !== 'availability'}
		<label class="block">
			<span class={labelClass}>Operator</span>
			<select class={inputClass} bind:value={operator}>
				<option value="greater_than">greater_than</option>
				<option value="less_than">less_than</option>
			</select>
		</label>
	{/if}
	{#if ruleType === 'threshold'}
		<label class="block">
			<span class={labelClass}>Threshold</span>
			<input class={inputClass} type="number" step="any" bind:value={threshold} />
		</label>
	{:else if ruleType === 'rate'}
		<label class="block">
			<span class={labelClass}>Threshold per second</span>
			<input class={inputClass} type="number" step="any" bind:value={thresholdPerSecond} />
		</label>
		<label class="block">
			<span class={labelClass}>Window (seconds, 30..=3600)</span>
			<input class={inputClass} type="number" min="30" max="3600" bind:value={windowSeconds} />
		</label>
	{/if}
	<label class="block">
		<span class={labelClass}>Dimension match key (optional)</span>
		<input class={inputClass} type="text" bind:value={dimensionKey} placeholder="mount_id" oninput={onDimensionEdit} />
	</label>
	<label class="block">
		<span class={labelClass}>Dimension match value</span>
		<input class={inputClass} type="text" bind:value={dimensionValue} placeholder="ext4:/" oninput={onDimensionEdit} />
	</label>
	{#if dimensionNote}
		<p class="sm:col-span-2 m-0 text-xs text-(--color-warning-dark)">{dimensionNote}</p>
	{/if}
{:else if ruleType === 'check_status'}
	<label class="block">
		<span class={labelClass}>Check id</span>
		<input class={inputClass} type="text" bind:value={checkId} placeholder="service:nginx.service" />
	</label>
	<label class="block">
		<span class={labelClass}>Status match</span>
		<select class={inputClass} bind:value={statusMatch}>
			<option value="critical">critical</option>
			<option value="warning">warning</option>
			<option value="unknown">unknown</option>
		</select>
	</label>
{:else if ruleType === 'group'}
	<div class="sm:col-span-2">
		<span class={labelClass}>Spec (group rules round-trip unchanged)</span>
		<p class="text-xs text-(--shell-text-secondary) m-0">{groupSummary}</p>
	</div>
{/if}
