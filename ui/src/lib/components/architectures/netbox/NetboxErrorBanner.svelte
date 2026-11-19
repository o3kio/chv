<script lang="ts">
	/**
	 * Inline `role="alert"` error banner shared by the NetBox panel's
	 * failure surfaces (config load, export, dry-run, retry, runs
	 * list). Presentational only: the caller supplies the heading and
	 * the already-code-branched message (the code-specific text mapping
	 * lives in ./types.ts), plus the BFF error code as a data attribute
	 * so tests and operators can distinguish failure classes.
	 */

	interface Props {
		heading: string;
		message: string;
		testId: string;
		/** BFF error code, when the source error carried one. */
		code?: string;
	}

	let { heading, message, testId, code }: Props = $props();
</script>

<div
	class="banner banner-error"
	role="alert"
	data-testid={testId}
	data-netbox-error-code={code}
>
	<strong>{heading}</strong>
	<span>{message}</span>
</div>

<style>
	.banner {
		display: flex;
		flex-direction: column;
		gap: 0.15rem;
		padding: 0.6rem 0.85rem;
		border-radius: var(--radius-xs);
		font-size: var(--chv-text-sm);
	}
	.banner-error {
		background: rgba(220, 38, 38, 0.08);
		border: 1px solid rgba(220, 38, 38, 0.4);
		color: rgb(153, 27, 27);
	}
</style>
