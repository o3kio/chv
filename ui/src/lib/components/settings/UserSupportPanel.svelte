<script lang="ts">
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import { ShieldCheck, Key, ShieldAlert } from 'lucide-svelte';
</script>

<aside class="support-area">
	<SectionCard title="Security Domain" icon={ShieldCheck}>
		<div class="domain-ops">
			<div class="safety-sign">
				<ShieldCheck size={16} />
				<span>RBAC_ENFORCED</span>
			</div>
			<p class="meta-hint">Identity federation is limited to local fabric database.</p>
		</div>
	</SectionCard>

	<SectionCard title="Fabric Roles" icon={Key}>
		<div class="role-matrix">
			<div class="role-item">
				<span class="tag warning">ADMIN</span>
				<span class="desc">UNRESTRICTED_ACCESS</span>
			</div>
			<div class="role-item">
				<span class="tag healthy">OPERATOR</span>
				<span class="desc">MUTATION_ACCESS</span>
			</div>
			<div class="role-item">
				<span class="tag neutral">VIEWER</span>
				<span class="desc">READ_ONLY_ACCESS</span>
			</div>
		</div>
	</SectionCard>

	<SectionCard title="Danger Zone" icon={ShieldAlert}>
		<p class="meta-hint">Account purges are permanent and logged in fabric audit.</p>
	</SectionCard>
</aside>

<style>
	.support-area {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.safety-sign {
		display: flex;
		align-items: center;
		gap: 0.75rem;
		padding: 0.75rem;
		background: rgba(var(--color-success-rgb), 0.1);
		color: var(--color-success);
		font-size: 10px;
		font-weight: 800;
		border-radius: var(--radius-xs);
	}

	.meta-hint {
		font-size: 10px;
		color: var(--chv-color-neutral-500);
		margin-top: 0.5rem;
		line-height: 1.4;
	}

	.role-matrix {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}

	.role-item {
		display: flex;
		align-items: center;
		gap: 0.75rem;
	}

	.tag {
		font-size: 9px;
		font-weight: 800;
		padding: 0.125rem 0.375rem;
		border-radius: 2px;
		min-width: 60px;
		text-align: center;
	}

	.tag.warning { background: rgba(var(--color-warning-rgb), 0.1); color: var(--color-warning); }
	.tag.healthy { background: rgba(var(--color-success-rgb), 0.1); color: var(--color-success); }
	.tag.neutral { background: var(--bg-surface-muted); color: var(--chv-color-neutral-500); }

	.role-item .desc {
		font-size: 9px;
		font-weight: 700;
		color: var(--chv-color-neutral-400);
	}
</style>
