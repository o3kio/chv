<script lang="ts">
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { isLoggedIn } from '#lib/api/client.ts';
  import { toast } from '#lib/stores/toast.svelte.ts';

  let username = '';
  let password = '';
  let error = '';
  let loading = false;

  // Redirect if the server has already established a session. The HttpOnly
  // cookie is not readable here; the non-credential hint cookie is.
  onMount(async () => {
    if (!isLoggedIn()) return;

    try {
      const response = await fetch('/v1/overview', {
        method: 'POST',
        credentials: 'include',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({})
      });
      if (response.ok) {
        await goto('/');
      }
    } catch {
      // Network or other error — stay on login page
    }
  });

  async function handleLogin() {
    if (!username || !password) {
      error = 'Username and password are required';
      return;
    }

    loading = true;
    error = '';

    try {
      const response = await fetch('/v1/auth/login', {
        method: 'POST',
        credentials: 'include',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ username, password })
      });

      if (!response.ok) {
        let message = `Login failed (${response.status})`;
        const contentType = response.headers.get('content-type');
        if (contentType && contentType.includes('application/json')) {
          try {
            const data = await response.json();
            message = data.error?.message || data.message || message;
          } catch {
            // ignore JSON parse error
          }
        }
        throw new Error(message);
      }

      const contentType = response.headers.get('content-type');
      if (!contentType || !contentType.includes('application/json')) {
        throw new Error('Login endpoint returned an unexpected response format.');
      }

      // The JWT is delivered only as an HttpOnly cookie (#660); the body
      // carries non-credential metadata. Nothing is stored client-side.
      const data = await response.json();

      if (data.must_change_password) {
        toast.info('Password change required. Please set a new password.');
        await goto('/change-password', {
          replace: true,
          refreshAll: true
        });
        return;
      }
      
      toast.success(`Welcome, ${data.user.username}!`);

      await goto('/', {
        replace: true,
        refreshAll: true
      });

      // Fall back to a hard navigation if the client router still leaves us on
      // the login screen after a successful auth response.
      if (typeof window !== 'undefined' && window.location.pathname === '/login') {
        window.location.replace('/');
      }
    } catch (err) {
      error = err instanceof Error ? err.message : 'Login failed';
      toast.error(error);
    } finally {
      loading = false;
    }
  }

  function handleKeydown(event: KeyboardEvent) {
    if (event.key === 'Enter') {
      handleLogin();
    }
  }
</script>

<div class="login-gateway">
  <div class="login-card">
    <header class="login-header">
      <div class="brand">
        <div class="brand-mark">CHV</div>
        <div class="brand-text">
          <span class="title">CellHV Infrastructure</span>
          <span class="subtitle">Cloud Hypervisor Platform</span>
        </div>
      </div>
      <h2 class="gateway-title">Identity Verification</h2>
    </header>

    <main class="login-body">
      <div class="input-group">
        <label for="username">Operator Identity</label>
        <div class="input-wrapper">
          <input
            id="username"
            bind:value={username}
            onkeydown={handleKeydown}
            placeholder="Username"
            autocomplete="username"
          />
        </div>
      </div>

      <div class="input-group">
        <label for="password">Access Credential</label>
        <div class="input-wrapper">
          <input
            id="password"
            bind:value={password}
            onkeydown={handleKeydown}
            type="password"
            placeholder="••••••••"
            autocomplete="current-password"
          />
        </div>
      </div>

      {#if error}
        <div class="login-error">
          <span class="error-mark">!</span>
          <span class="error-msg">{error}</span>
        </div>
      {/if}

      <button
        type="button"
        class="btn-login"
        onclick={(e) => { e.preventDefault(); handleLogin(); }}
        disabled={loading}
      >
        {#if loading}
          <div class="loading-dots">
            <span></span><span></span><span></span>
          </div>
        {:else}
          Authenticate Session
        {/if}
      </button>

      <footer class="login-footer">
        <p>Technical access is logged and audited. Unauthorized attempts are prohibited.</p>
      </footer>
    </main>
  </div>
</div>

<style>
  .login-gateway {
    min-height: 100vh;
    display: grid;
    place-items: center;
    background-color: var(--bg-base);
    background-image: 
      radial-gradient(var(--dot-grid) 1px, transparent 0),
      radial-gradient(var(--dot-grid) 1px, transparent 0);
    background-position: 0 0, 12px 12px;
    background-size: 24px 24px;
    padding: 1.5rem;
  }

  .login-card {
    width: 100%;
    max-width: 400px;
    background: var(--bg-surface);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm);
    box-shadow: 0 20px 50px rgba(0, 0, 0, 0.3);
    overflow: hidden;
  }

  .login-header {
    padding: 2rem;
    background: var(--bg-surface-muted);
    border-bottom: 1px solid var(--border-subtle);
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 1rem;
    margin-bottom: 1.5rem;
  }

  .brand-mark {
    width: 3rem;
    height: 3rem;
    display: grid;
    place-items: center;
    background: var(--color-primary);
    color: #ffffff;
    font-size: 1rem;
    font-weight: 800;
    border-radius: var(--radius-sm);
    letter-spacing: 0.1em;
  }

  .brand-text {
    display: flex;
    flex-direction: column;
  }

  .brand-text .title {
    font-size: 1.1rem;
    font-weight: 700;
    color: var(--color-neutral-900);
  }

  .brand-text .subtitle {
    font-size: 0.65rem;
    text-transform: uppercase;
    font-weight: 600;
    color: var(--color-neutral-400);
    letter-spacing: 0.05em;
  }

  .gateway-title {
    font-size: 0.75rem;
    text-transform: uppercase;
    font-weight: 700;
    color: var(--color-neutral-500);
    letter-spacing: 0.1em;
    margin: 0;
  }

  .login-body {
    padding: 2rem;
    display: flex;
    flex-direction: column;
    gap: 1.5rem;
  }

  .input-group {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }

  .input-group label {
    font-size: 10px;
    text-transform: uppercase;
    font-weight: 700;
    color: var(--color-neutral-500);
    letter-spacing: 0.05em;
  }

  .input-wrapper input {
    width: 100%;
    background: var(--bg-surface-muted);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-xs);
    padding: 0.75rem 1rem;
    font-size: var(--text-sm);
    color: var(--color-neutral-900);
    transition: all 0.2s ease;
  }

  .input-wrapper input:focus {
    outline: none;
    border-color: var(--color-primary);
    background: var(--bg-surface);
    box-shadow: 0 0 0 3px var(--color-primary-light);
  }

  .login-error {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    padding: 0.75rem;
    background: var(--color-danger-light);
    border: 1px solid var(--color-danger);
    border-radius: var(--radius-xs);
  }

  .error-mark {
    width: 1rem;
    height: 1rem;
    display: grid;
    place-items: center;
    background: var(--color-danger);
    color: #ffffff;
    font-weight: 800;
    font-size: 10px;
    border-radius: 50%;
  }

  .error-msg {
    font-size: 11px;
    font-weight: 600;
    color: var(--color-danger);
  }

  .btn-login {
    width: 100%;
    background: var(--color-primary);
    color: #ffffff;
    border: none;
    padding: 0.9rem;
    font-size: 0.8rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    border-radius: var(--radius-sm);
    cursor: pointer;
    transition: all 0.2s ease;
    display: grid;
    place-items: center;
  }

  .btn-login:hover:not(:disabled) {
    background: var(--color-primary-dark);
    transform: translateY(-1px);
    box-shadow: 0 4px 12px var(--color-primary-light);
  }

  .btn-login:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .login-footer {
    border-top: 1px solid var(--border-subtle);
    padding-top: 1.5rem;
    text-align: center;
  }

  .login-footer p {
    font-size: 10px;
    color: var(--color-neutral-400);
    line-height: 1.5;
    margin: 0;
  }

  /* Loading State */
  .loading-dots {
    display: flex;
    gap: 4px;
  }

  .loading-dots span {
    width: 5px;
    height: 5px;
    background: #ffffff;
    border-radius: 50%;
    animation: bounce 1.4s infinite ease-in-out both;
  }

  .loading-dots span:nth-child(1) { animation-delay: -0.32s; }
  .loading-dots span:nth-child(2) { animation-delay: -0.16s; }

  @keyframes bounce {
    0%, 80%, 100% { transform: scale(0); }
    40% { transform: scale(1); }
  }
</style>
