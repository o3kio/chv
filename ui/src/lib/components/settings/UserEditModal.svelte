<script lang="ts">
  import Modal from '#lib/components/primitives/Modal.svelte';
  import Button from '#lib/components/primitives/Button.svelte';
  import { ShieldAlert } from 'lucide-svelte';
  import type { UserItem } from '#lib/bff/users.ts';

  interface Props {
    user?: UserItem | null;
    open?: boolean;
    submitting?: boolean;
    error?: string;
    onsave?: (payload: { username?: string; password: string; role: string; display_name: string; email: string }) => void;
    onclose?: () => void;
  }

  let {
    user = null,
    open = $bindable(false),
    submitting = false,
    error = '',
    onsave,
    onclose
  }: Props = $props();

  const isCreate = $derived(user === null);
  const prefix = $derived(isCreate ? 'create' : 'edit');

  let username = $state('');
  let password = $state('');
  let role = $state('viewer');
  let display_name = $state('');
  let email = $state('');

  $effect(() => {
    if (open) {
      if (user) {
        username = user.username;
        password = '';
        role = user.role;
        display_name = user.display_name ?? '';
        email = user.email ?? '';
      } else {
        username = '';
        password = '';
        role = 'viewer';
        display_name = '';
        email = '';
      }
    }
  });

  function handleSave() {
    if (submitting) return;
    if (onsave) {
      if (isCreate) {
        onsave({username: username.trim(), password, role, display_name, email});
      } else {
        onsave({password, role, display_name, email});
      }
    }
  }
</script>

<Modal bind:open title={isCreate ? 'NEW_IDENTITY_ENTRY' : 'MODIFY_IDENTITY_ENTRY'}>
{#snippet children()}
  <div class="params-column">
    <div class="field-control">
      <label class="label" for="{prefix}-tech-username">IDENTITY_ID</label>
      {#if isCreate}
        <input id="{prefix}-tech-username" type="text" bind:value={username} placeholder="operator_id" />
      {:else}
        <input id="{prefix}-tech-username" type="text" value={username} disabled class="locked" />
      {/if}
    </div>
    <div class="field-control">
      <label class="label" for="{prefix}-tech-password">{isCreate ? 'ACCESS_PWD' : 'RESET_PWD'}</label>
      <input id="{prefix}-tech-password" type="password" bind:value={password} placeholder={isCreate ? 'min_8_chars' : 'leave_blank_to_keep'} />
    </div>
    <div class="field-control">
      <label class="label" for="{prefix}-tech-role">RBAC_ROLE</label>
      <select id="{prefix}-tech-role" bind:value={role}>
        <option value="viewer">VIEWER</option>
        <option value="operator">OPERATOR</option>
        <option value="admin">ADMIN</option>
      </select>
    </div>
    <div class="field-control">
      <label class="label" for="{prefix}-tech-display-name">ALIAS</label>
      <input id="{prefix}-tech-display-name" type="text" bind:value={display_name} placeholder={isCreate ? 'optional_name' : ''} />
    </div>
    <div class="field-control">
      <label class="label" for="{prefix}-tech-email">EMAIL_LINK</label>
      <input id="{prefix}-tech-email" type="email" bind:value={email} placeholder={isCreate ? 'optional_mail' : ''} />
    </div>
    {#if error}
      <div class="form-error-row">
        <ShieldAlert size={12} />
        <span>{error}</span>
      </div>
    {/if}
  </div>
{/snippet}
{#snippet footer()}
  <Button onclick={onclose} variant="secondary">ABORT</Button>
  <Button onclick={handleSave} disabled={submitting}  variant="primary">
    {submitting ? 'EXECUTING...' : (isCreate ? 'COMMIT_ENTRY' : 'PATCH_ENTRY')}
  </Button>
{/snippet}
</Modal>

<!-- Plain styled modal -->
<Modal bind:open title={isCreate ? 'Create User' : 'Edit User'}>
{#snippet children()}
  <form onsubmit={(e) => { e.preventDefault(); handleSave(); }} class="form-fields">
    <div class="field">
      <label for="{prefix}-username">Username</label>
      {#if isCreate}
        <input id="{prefix}-username" type="text" bind:value={username} placeholder="e.g. jdoe" autocomplete="off" />
      {:else}
        <input id="{prefix}-username" type="text" value={username} disabled class="disabled-input" />
      {/if}
    </div>
    <div class="field">
      <label for="{prefix}-password">Password</label>
      <input id="{prefix}-password" type="password" bind:value={password} placeholder="Min 8 characters" autocomplete="new-password" />
    </div>
    <div class="field">
      <label for="{prefix}-role">Role</label>
      <select id="{prefix}-role" bind:value={role}>
        <option value="viewer">Viewer</option>
        <option value="operator">Operator</option>
        <option value="admin">Admin</option>
      </select>
    </div>
    <div class="field">
      <label for="{prefix}-display-name">Display Name {isCreate ? '(optional)' : ''}</label>
      <input id="{prefix}-display-name" type="text" bind:value={display_name} placeholder="e.g. Jane Doe" autocomplete="off" />
    </div>
    <div class="field">
      <label for="{prefix}-email">Email {isCreate ? '(optional)' : ''}</label>
      <input id="{prefix}-email" type="email" bind:value={email} placeholder="user@example.com" autocomplete="off" />
    </div>
    {#if error}
      <div class="form-error">{error}</div>
    {/if}
  </form>
{/snippet}
{#snippet footer()}
  <Button type="button" onclick={onclose} variant="secondary">Cancel</Button>
  <Button type="button" onclick={handleSave} disabled={submitting}  variant="primary">
    {submitting ? (isCreate ? 'Creating...' : 'Saving...') : (isCreate ? 'Create User' : 'Save Changes')}
  </Button>
{/snippet}
</Modal>

<style>
  .params-column {
    display: flex;
    flex-direction: column;
    gap: 1rem;
    padding: 0.5rem 0;
  }

  .field-control {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }

  .field-control .label {
    font-size: 10px;
    font-weight: 800;
    color: var(--chv-color-neutral-500);
  }

  .field-control input, .field-control select {
    background: var(--bg-surface-muted);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-xs);
    padding: 0.4rem 0.625rem;
    font-size: 11px;
    font-family: var(--chv-font-mono);
    color: var(--chv-color-neutral-900);
    width: 100%;
  }

  .field-control input.locked {
    opacity: 0.6;
    background: var(--bg-surface-muted);
    cursor: not-allowed;
  }

  .form-error-row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.5rem;
    background: rgba(var(--color-danger-rgb), 0.1);
    color: var(--color-danger);
    font-size: 10px;
    font-weight: 800;
    border-radius: 2px;
  }

  .form-fields {
    display: flex;
    flex-direction: column;
    gap: 1rem;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 0.375rem;
  }

  .field label {
    font-size: var(--chv-text-xs);
    font-weight: 600;
    color: var(--shell-text-muted);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }

  .field input,
  .field select {
    padding: 0.5rem 0.75rem;
    border: 1px solid var(--shell-line-strong);
    border-radius: 0.25rem;
    background: var(--shell-surface);
    color: var(--shell-text);
    font-size: var(--chv-text-sm);
    outline: none;
    transition: border-color 0.15s;
  }

  .field input:focus,
  .field select:focus {
    border-color: var(--shell-accent);
  }

  .disabled-input {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .form-error {
    padding: 0.5rem 0.75rem;
    background: var(--status-failed-bg);
    border: 1px solid var(--status-failed-border);
    border-radius: 0.25rem;
    color: var(--status-failed-text);
    font-size: var(--chv-text-sm);
  }
</style>
