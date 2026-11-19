<script lang="ts">
	import { SlidersHorizontal, X, ChevronDown } from 'lucide-svelte';
	import { slide } from 'svelte/transition';

	export interface FilterOption {
		key: string;
		label: string;
		type: 'select' | 'text' | 'date' | 'boolean';
		options?: { value: string; label: string }[];
		placeholder?: string;
	}

	interface Props {
		filters: FilterOption[];
		activeFilters: Record<string, unknown>;
		onFilterChange: (key: string, value: unknown) => void;
		onClearAll: () => void;
	}

	let { filters, activeFilters, onFilterChange, onClearAll }: Props = $props();

	// Track expanded state for mobile
	let isExpanded = $state(false);

	// Get active filter count
	const activeCount = $derived(
		Object.values(activeFilters).filter(v => v !== undefined && v !== null && v !== '').length
	);

	// Get display value for a filter
	function getFilterDisplayValue(key: string, value: unknown): string {
		const filter = filters.find(f => f.key === key);
		if (!filter) return String(value);

		if (filter.type === 'select' && filter.options) {
			const option = filter.options.find(o => o.value === value);
			return option?.label ?? String(value);
		}

		if (filter.type === 'boolean') {
			return value ? 'Yes' : 'No';
		}

		return String(value);
	}

	// Get active filter entries
	const activeFilterEntries = $derived(
		Object.entries(activeFilters).filter(([_, v]) => v !== undefined && v !== null && v !== '')
	);

	function handleFilterChange(key: string, e: Event) {
		const target = e.target as HTMLInputElement | HTMLSelectElement;
		let value: unknown = target.value;

		// Handle checkbox for boolean type
		if (target.type === 'checkbox') {
			value = (target as HTMLInputElement).checked;
		}

		onFilterChange(key, value);
	}

	function clearFilter(key: string) {
		onFilterChange(key, '');
	}
</script>

<div class="filter-bar">
	<!-- Mobile toggle -->
	<button
		type="button"
		class="filter-toggle"
		onclick={() => isExpanded = !isExpanded}
		aria-expanded={isExpanded}
	>
		<SlidersHorizontal size={16} />
		<span>Filters</span>
		{#if activeCount > 0}
			<span class="filter-badge">{activeCount}</span>
		{/if}
		<span class="chevron" class:rotated={isExpanded}>
			<ChevronDown size={16} />
		</span>
	</button>

	<!-- Filter content -->
	<div class="filter-content" class:expanded={isExpanded}>
		<!-- Active filter chips -->
		{#if activeCount > 0}
			<div class="filter-chips" transition:slide={{ duration: 150 }}>
				{#each activeFilterEntries as [key, value]}
					<span class="filter-chip">
						<span class="chip-label">{getFilterDisplayValue(key, value)}</span>
						<button
							type="button"
							class="chip-remove"
							onclick={() => clearFilter(key)}
							aria-label={`Remove ${key} filter`}
						>
							<X size={12} />
						</button>
					</span>
				{/each}
				<button
					type="button"
					class="clear-all-btn"
					onclick={onClearAll}
				>
					Clear all
				</button>
			</div>
		{/if}

		<!-- Filter inputs -->
		<div class="filter-inputs">
			{#each filters as filter}
				<div class="filter-field">
					<label for={`filter-${filter.key}`} class="filter-label">
						{filter.label}
					</label>
					
					{#if filter.type === 'select'}
						<select
							id={`filter-${filter.key}`}
							class="filter-select"
							value={activeFilters[filter.key] ?? ''}
							onchange={(e) => handleFilterChange(filter.key, e)}
						>
							<option value="">All</option>
							{#each filter.options ?? [] as option}
								<option value={option.value}>{option.label}</option>
							{/each}
						</select>
					{:else if filter.type === 'text'}
						<input
							type="text"
							id={`filter-${filter.key}`}
							class="filter-input"
							placeholder={filter.placeholder ?? `Filter by ${filter.label}`}
							value={activeFilters[filter.key] ?? ''}
							oninput={(e) => handleFilterChange(filter.key, e)}
						/>
					{:else if filter.type === 'date'}
						<input
							type="date"
							id={`filter-${filter.key}`}
							class="filter-input"
							value={activeFilters[filter.key] ?? ''}
							onchange={(e) => handleFilterChange(filter.key, e)}
						/>
					{:else if filter.type === 'boolean'}
						<label class="filter-checkbox">
							<input
								type="checkbox"
								checked={!!activeFilters[filter.key]}
								onchange={(e) => handleFilterChange(filter.key, e)}
							/>
							<span>{filter.label}</span>
						</label>
					{/if}
				</div>
			{/each}
		</div>
	</div>
</div>

<style>
	.filter-bar {
		background: white;
		border-bottom: 1px solid var(--chv-color-neutral-200);
	}

	.filter-toggle {
		display: none;
		align-items: center;
		gap: 0.5rem;
		width: 100%;
		padding: 0.75rem 1rem;
		background: transparent;
		border: none;
		color: var(--chv-color-neutral-700);
		font-size: var(--chv-text-sm);
		font-weight: 500;
		cursor: pointer;
	}

	.filter-toggle:hover {
		background: var(--chv-color-neutral-50);
	}

	.filter-badge {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		min-width: 1.25rem;
		height: 1.25rem;
		padding: 0 0.375rem;
		background: var(--color-primary);
		color: white;
		font-size: var(--chv-text-xs);
		font-weight: 600;
		border-radius: 9999px;
	}

	.chevron {
		transition: transform var(--duration-fast) var(--ease-default);
		margin-left: auto;
	}

	.chevron.rotated {
		transform: rotate(180deg);
	}

	.filter-content {
		padding: 1rem;
	}

	.filter-content:not(.expanded) {
		display: block;
	}

	.filter-chips {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.5rem;
		margin-bottom: 1rem;
		padding-bottom: 1rem;
		border-bottom: 1px solid var(--chv-color-neutral-100);
	}

	.filter-chip {
		display: inline-flex;
		align-items: center;
		gap: 0.25rem;
		padding: 0.25rem 0.5rem;
		background: var(--color-primary-light);
		border: 1px solid rgba(229, 112, 53, 0.2);
		border-radius: var(--radius-sm);
		font-size: var(--chv-text-xs);
		color: var(--color-primary-dark);
	}

	.chip-label {
		font-weight: 500;
	}

	.chip-remove {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		padding: 0.125rem;
		background: transparent;
		border: none;
		border-radius: var(--radius-sm);
		color: var(--color-primary-dark);
		cursor: pointer;
		opacity: 0.7;
		transition: opacity var(--duration-fast);
	}

	.chip-remove:hover {
		opacity: 1;
		background: rgba(229, 112, 53, 0.15);
	}

	.clear-all-btn {
		padding: 0.25rem 0.5rem;
		background: transparent;
		border: none;
		border-radius: var(--radius-sm);
		color: var(--chv-color-neutral-500);
		font-size: var(--chv-text-xs);
		font-weight: 500;
		cursor: pointer;
		transition: all var(--duration-fast);
	}

	.clear-all-btn:hover {
		color: var(--color-danger);
		background: var(--color-danger-light);
	}

	.filter-inputs {
		display: flex;
		flex-wrap: wrap;
		gap: 1rem;
	}

	.filter-field {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		min-width: 150px;
		flex: 1;
	}

	.filter-label {
		font-size: var(--chv-text-xs);
		font-weight: 500;
		color: var(--chv-color-neutral-600);
		text-transform: uppercase;
		letter-spacing: 0.05em;
	}

	.filter-select,
	.filter-input {
		padding: 0.5rem 0.75rem;
		border: 1px solid var(--chv-color-neutral-200);
		border-radius: var(--radius-sm);
		font-size: var(--chv-text-sm);
		background: white;
		color: var(--chv-color-neutral-700);
		transition: border-color var(--duration-fast);
	}

	.filter-select:focus,
	.filter-input:focus {
		outline: none;
		border-color: var(--color-primary);
		box-shadow: 0 0 0 3px rgba(229, 112, 53, 0.1);
	}

	.filter-select {
		appearance: none;
		background-image: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='16' height='16' viewBox='0 0 24 24' fill='none' stroke='%2364748b' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'%3E%3Cpath d='m6 9 6 6 6-6'/%3E%3C/svg%3E");
		background-repeat: no-repeat;
		background-position: right 0.5rem center;
		padding-right: 2rem;
	}

	.filter-checkbox {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		padding: 0.5rem 0;
		cursor: pointer;
		font-size: var(--chv-text-sm);
		color: var(--chv-color-neutral-700);
	}

	.filter-checkbox input[type='checkbox'] {
		width: 1rem;
		height: 1rem;
		accent-color: var(--color-primary);
		cursor: pointer;
	}

	/* Responsive */
	@media (max-width: 768px) {
		.filter-toggle {
			display: flex;
		}

		.filter-content {
			display: none;
			padding: 0.75rem;
		}

		.filter-content.expanded {
			display: block;
		}

		.filter-inputs {
			flex-direction: column;
		}

		.filter-field {
			min-width: 100%;
		}
	}
</style>
