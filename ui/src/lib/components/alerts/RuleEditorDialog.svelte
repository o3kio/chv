<script lang="ts">
	import Modal from '#lib/components/primitives/Modal.svelte';
	import Button from '#lib/components/primitives/Button.svelte';
	import Badge from '#lib/components/primitives/Badge.svelte';
	import { RULE_TEMPLATES, ruleSpecSummary, type RuleTemplate } from '#lib/alerts/rules.ts';
	import RuleTemplatePicker from './RuleTemplatePicker.svelte';
	import RuleSpecFields from './RuleSpecFields.svelte';
	import type {
		AlertRule,
		RuleMutateBody
	} from '#lib/bff/alerting.ts';

	interface Props {
		open?: boolean;
		/** Edit mode is initialized from `rule`; create mode from a template. */
		rule?: AlertRule | null;
		onClose: () => void;
		onSubmit: (body: RuleMutateBody) => Promise<void>;
	}

	let { open = $bindable(false), rule = null, onClose, onSubmit }: Props = $props();

	const isEdit = $derived(rule !== null);
	let selectedTemplate = $state<RuleTemplate | null>(null);

	// ---- form state ----
	let name = $state('');
	let targetKind = $state<'node' | 'vm'>('node');
	let targetId = $state('');
	let severity = $state<'critical' | 'warning' | 'info'>('warning');
	let forSeconds = $state(300);
	let recoverySeconds = $state(120);
	let missingData = $state<'unknown' | 'fire' | 'ignore'>('unknown');
	// Templates NEVER auto-enable: create mode starts unchecked and the
	// operator must flip it after reviewing the pre-filled values.
	let enabled = $state(false);
	let metricId = $state('');
	let operator = $state<'greater_than' | 'less_than'>('greater_than');
	let threshold = $state(0);
	let thresholdPerSecond = $state(0);
	let windowSeconds = $state(300);
	let checkId = $state('');
	let statusMatch = $state<'critical' | 'warning' | 'unknown'>('critical');
	let dimensionKey = $state('');
	let dimensionValue = $state('');
	// The original rule's full dimension_match (edit mode). The dialog
	// edits ONE key, but a rule can carry two — rebuilding the match
	// from the two state fields would silently drop a second key the
	// operator never touched. `dimensionTouched` distinguishes an
	// untouched round-trip from an explicit edit.
	let originalDimensions: Record<string, string> | undefined = $state();
	let dimensionTouched = $state(false);
	let submitting = $state(false);
	let validationError = $state('');

	const ruleType = $derived<'threshold' | 'rate' | 'availability' | 'check_status' | 'group'>(
		isEdit ? (rule?.rule_type ?? 'threshold') : (selectedTemplate?.rule_type ?? 'threshold')
	);

	// Editing a rule whose match carries more than the one key the
	// dialog can edit: the untouched form cannot be saved through this
	// dialog (no registered metric declares two dimensions, so the
	// server rejects it loudly); editing the dimension fields replaces
	// the whole match with the single key shown.
	const dimensionNote = $derived.by(() => {
		if (!isEdit || !originalDimensions) return '';
		const keys = Object.keys(originalDimensions);
		if (keys.length < 2) return '';
		return `This rule matches ${keys.length} dimensions (${keys.join(', ')}). That form cannot be saved through this dialog — untouched it round-trips verbatim and fails loudly on save; edit the fields to replace the match with the single key shown, or cancel to leave the rule untouched.`;
	});

	function applyTemplate(template: RuleTemplate) {
		selectedTemplate = template;
		name = template.title;
		targetKind = template.target_kind;
		severity = template.severity;
		forSeconds = template.for_seconds;
		recoverySeconds = template.recovery_seconds;
		missingData = template.missing_data;
		enabled = false;
		metricId = template.spec.metric_id ?? '';
		operator = template.spec.operator ?? 'greater_than';
		threshold = template.spec.threshold ?? 0;
		thresholdPerSecond = template.spec.threshold_per_second ?? 0;
		windowSeconds = template.spec.window_seconds ?? 300;
		checkId = template.spec.check_id ?? '';
		statusMatch = template.spec.status_match ?? 'critical';
		const dimensions = template.spec.dimension_match ?? {};
		dimensionKey = Object.keys(dimensions)[0] ?? '';
		dimensionValue = dimensions[dimensionKey] ?? '';
		originalDimensions = undefined;
		dimensionTouched = false;
	}

	function applyRule(source: AlertRule) {
		name = source.name;
		targetKind = source.target_kind;
		targetId = source.target_id;
		severity = source.severity;
		forSeconds = source.for_seconds;
		recoverySeconds = source.recovery_seconds;
		missingData = source.missing_data;
		enabled = source.enabled;
		metricId = source.metric_id ?? '';
		operator = source.operator ?? 'greater_than';
		threshold = source.threshold ?? 0;
		thresholdPerSecond = source.threshold_per_second ?? 0;
		windowSeconds = source.window_seconds ?? 300;
		checkId = source.check_id ?? '';
		statusMatch = source.status_match ?? 'critical';
		const dimensions = source.dimension_match ?? {};
		dimensionKey = Object.keys(dimensions)[0] ?? '';
		dimensionValue = dimensions[dimensionKey] ?? '';
		originalDimensions = source.dimension_match;
		dimensionTouched = false;
	}

	// Initialize whenever the dialog opens for a new target.
	$effect(() => {
		if (!open) return;
		validationError = '';
		targetId = '';
		if (rule) {
			applyRule(rule);
		} else {
			// Create mode starts on the first starter template; the
			// operator reviews and adjusts before creating.
			applyTemplate(RULE_TEMPLATES[0]);
		}
	});

	function dimensionMatch(): Record<string, string> | undefined {
		if (!dimensionKey.trim() || !dimensionValue.trim()) return undefined;
		// Untouched multi-key match: round-trip the ORIGINAL verbatim
		// so common-field-only edits never silently narrow it (the
		// server will reject the multi-key form loudly — no registered
		// metric declares two dimensions — which is the honest outcome
		// for a legacy row). Editing the fields replaces the match
		// with the single key shown, exactly as the note says.
		const original = originalDimensions;
		if (
			!dimensionTouched &&
			original &&
			Object.keys(original).length > 1 &&
			original[dimensionKey.trim()] === dimensionValue.trim()
		) {
			return { ...original };
		}
		return { [dimensionKey.trim()]: dimensionValue.trim() };
	}

	function validate(): string {
		if (!name.trim()) return 'Name is required.';
		if (!targetId.trim()) return 'Target id is required (v1 rules bind exactly one target).';
		if (forSeconds < 1 || recoverySeconds < 1) return 'Hold and recovery durations must be at least 1 second.';
		if (ruleType === 'threshold' || ruleType === 'rate' || ruleType === 'availability') {
			if (!metricId.trim()) return 'Metric id is required.';
		}
		if (ruleType === 'threshold' && !Number.isFinite(threshold)) return 'Threshold must be a number.';
		if (ruleType === 'rate') {
			if (!Number.isFinite(thresholdPerSecond)) return 'Rate threshold must be a number.';
			if (windowSeconds < 30 || windowSeconds > 3600) return 'Rate window must be 30..=3600 seconds.';
		}
		if (ruleType === 'check_status' && !checkId.trim()) return 'Check id is required.';
		return '';
	}

	async function submit() {
		validationError = validate();
		if (validationError) return;
		const body: RuleMutateBody = {
			name: name.trim(),
			target_kind: targetKind,
			target_id: targetId.trim(),
			severity,
			for_seconds: forSeconds,
			recovery_seconds: recoverySeconds,
			missing_data: missingData,
			enabled
		};
		switch (ruleType) {
			case 'threshold':
				body.metric_id = metricId.trim();
				body.operator = operator;
				body.threshold = threshold;
				body.dimension_match = dimensionMatch();
				break;
			case 'rate':
				body.metric_id = metricId.trim();
				body.operator = operator;
				body.threshold_per_second = thresholdPerSecond;
				body.window_seconds = windowSeconds;
				body.dimension_match = dimensionMatch();
				break;
			case 'availability':
				body.metric_id = metricId.trim();
				body.dimension_match = dimensionMatch();
				break;
			case 'check_status':
				body.check_id = checkId.trim();
				body.status_match = statusMatch;
				break;
			case 'group':
				// Group specs round-trip untouched; only common fields edit.
				body.op = rule?.op;
				body.conditions = rule?.conditions;
				break;
		}
		if (isEdit && rule) {
			body.rule_id = rule.rule_id;
			body.expected_revision = rule.revision;
		}
		submitting = true;
		try {
			await onSubmit(body);
			open = false;
		} catch {
			// Conflict and validation errors are surfaced by the caller
			// (toast + reload); the dialog stays out of the way.
		} finally {
			submitting = false;
		}
	}

	const inputClass =
		'w-full px-2.5 py-1.5 text-sm border border-(--chv-color-neutral-300) rounded-xs bg-(--bg-surface) text-(--chv-color-neutral-900)';
	const labelClass =
		'block text-[10px] font-bold uppercase tracking-wider text-(--shell-text-muted) mb-1';
</script>

<Modal bind:open title={isEdit ? 'Edit alert rule' : 'New alert rule'} width="wide">
	{#if !isEdit}
		<RuleTemplatePicker selectedId={selectedTemplate?.id ?? null} onSelect={applyTemplate} />
	{/if}

	<div class="grid grid-cols-1 sm:grid-cols-2 gap-3">
		<label class="block sm:col-span-2">
			<span class={labelClass}>Name</span>
			<input class={inputClass} type="text" bind:value={name} />
		</label>
		<label class="block">
			<span class={labelClass}>Target kind</span>
			<select class={inputClass} bind:value={targetKind}>
				<option value="node">node</option>
				<option value="vm">vm</option>
			</select>
		</label>
		<label class="block">
			<span class={labelClass}>Target id</span>
			<input class={inputClass} type="text" bind:value={targetId} placeholder={targetKind === 'node' ? 'node id' : 'vm id'} />
		</label>
		<label class="block">
			<span class={labelClass}>Severity</span>
			<select class={inputClass} bind:value={severity}>
				<option value="critical">critical</option>
				<option value="warning">warning</option>
				<option value="info">info</option>
			</select>
		</label>
		<label class="block">
			<span class={labelClass}>Missing data</span>
			<select class={inputClass} bind:value={missingData}>
				<option value="unknown">unknown</option>
				<option value="fire">fire</option>
				<option value="ignore">ignore</option>
			</select>
		</label>
		<label class="block">
			<span class={labelClass}>For (seconds)</span>
			<input class={inputClass} type="number" min="1" bind:value={forSeconds} />
		</label>
		<label class="block">
			<span class={labelClass}>Recovery (seconds)</span>
			<input class={inputClass} type="number" min="1" bind:value={recoverySeconds} />
		</label>

		<RuleSpecFields
			{ruleType}
			groupSummary={rule ? ruleSpecSummary(rule) : ''}
			dimensionNote={dimensionNote}
			onDimensionEdit={() => (dimensionTouched = true)}
			bind:metricId
			bind:operator
			bind:threshold
			bind:thresholdPerSecond
			bind:windowSeconds
			bind:checkId
			bind:statusMatch
			bind:dimensionKey
			bind:dimensionValue
		/>

		<label class="flex items-center gap-2 text-sm text-(--chv-color-neutral-700) sm:col-span-2 mt-1 cursor-pointer">
			<input type="checkbox" bind:checked={enabled} />
			Enabled
			{#if !isEdit}
				<Badge variant="warning">review before enabling</Badge>
			{/if}
		</label>
	</div>

	{#if validationError}
		<p class="text-xs text-danger mt-3 mb-0" role="alert">{validationError}</p>
	{/if}

	{#snippet footer()}
		<Button variant="secondary" onclick={onClose} disabled={submitting}>Cancel</Button>
		<Button variant="primary" onclick={submit} loading={submitting}>
			{isEdit ? 'Save rule' : 'Create rule'}
		</Button>
	{/snippet}
</Modal>
