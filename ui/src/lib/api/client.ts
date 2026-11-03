import { PUBLIC_CHV_API_BASE_URL } from '$app/env/public';
import { goto } from '$app/navigation';
import { toast } from '#lib/stores/toast.svelte.ts';
import type {
  APIErrorEnvelope,
  Event,
  InstallActionResponse,
  InstallStatusResponse,
  NodeWithResources,
  VM,
  VMTemplate,
  CreateVMTemplateInput,
  CloneFromTemplateInput,
  CloudInitTemplate,
  CreateCloudInitTemplateInput,
  RenderCloudInitTemplateInput,
  RenderCloudInitTemplateResponse,
  Quota,
  UsageWithQuota,
  SetQuotaInput,
  UpdateQuotaInput,
} from '#lib/api/types.ts';

const DEFAULT_BASE_URL = PUBLIC_CHV_API_BASE_URL || ''; // Empty string means same origin

/** Non-credential UX hint cookie set by the BFF at login. Grants nothing. */
const AUTH_HINT_COOKIE = 'chv_logged_in';
/** Non-credential role hint cookie set by the BFF at login. Grants nothing. */
const ROLE_COOKIE = 'chv_role';

function readCookie(name: string): string | null {
  if (typeof document === 'undefined' || typeof document.cookie !== 'string') {
    return null;
  }
  const prefix = `${name}=`;
  for (const pair of document.cookie.split(';')) {
    const trimmed = pair.trim();
    if (trimmed.startsWith(prefix)) {
      return decodeURIComponent(trimmed.slice(prefix.length));
    }
  }
  return null;
}

function deleteCookie(name: string): void {
  if (typeof document === 'undefined') return;
  document.cookie = `${name}=; Max-Age=0; Path=/; SameSite=Strict`;
}

/**
 * HTTP-only cookie sessions mean there is no bearer token for scripts to
 * read. This remains as a compatibility shim for callers that used to pass
 * a token through; the BFF client sends cookies automatically.
 */
export function getStoredToken(): string | null {
  return null;
}

/**
 * No-op retained for API compatibility. Browser sessions are established by
 * the server's HttpOnly `chv_session` cookie during `/v1/auth/login`; a token
 * must never be written to localStorage or sessionStorage (#660).
 */
export function storeToken(_token: string): void {
  // Intentionally empty: the browser has no access to the session credential.
}

/**
 * Clear the non-credential UX hint cookies. The HttpOnly session credential
 * cannot be read or cleared by script: callers logging out should also call
 * the server's `/v1/auth/logout` (the nav does this) so the JWT is revoked
 * and the cookie is expired.
 */
export function clearToken(): void {
  deleteCookie(AUTH_HINT_COOKIE);
  deleteCookie(ROLE_COOKIE);
}

/** True when the server has set the authenticated-session hint cookie. */
export function isLoggedIn(): boolean {
  return readCookie(AUTH_HINT_COOKIE) === '1';
}

/**
 * Role hint from the non-credential `chv_role` cookie. Used only to hide
 * controls the server already gates; never an authorization decision.
 */
export function getStoredRole(): string | null {
  return readCookie(ROLE_COOKIE);
}

/**
 * Custom API error class that preserves error details from the server.
 */
export class APIError extends Error {
  public readonly status: number;
  public readonly code: string;
  public readonly retryable: boolean;
  public readonly hint?: string;

  constructor(
    message: string,
    status: number,
    code: string,
    retryable: boolean = false,
    hint?: string
  ) {
    super(message);
    this.name = 'APIError';
    this.status = status;
    this.code = code;
    this.retryable = retryable;
    this.hint = hint;
  }
}

/**
 * Check if an error is a network error (fetch failed to connect).
 */
function isNetworkError(error: unknown): boolean {
  return error instanceof TypeError && 
    (error.message.includes('fetch') || 
     error.message.includes('Network') ||
     error.message.includes('Failed to fetch'));
}

/**
 * Get a user-friendly error message based on the error type.
 */
function getUserFriendlyMessage(error: unknown): string {
  if (error instanceof APIError) {
    // Return the server message for API errors
    return error.message;
  }
  
  if (error instanceof TypeError && isNetworkError(error)) {
    return 'Unable to connect to the server. Please check your network connection and try again.';
  }
  
  if (error instanceof Error) {
    return error.message;
  }
  
  return 'An unexpected error occurred. Please try again.';
}

function getHeader(response: Response, name: string): string | null {
  return response.headers?.get?.(name) ?? null;
}

function isJsonResponse(response: Response): boolean {
  const contentType = getHeader(response, 'content-type');
  return contentType?.toLowerCase().includes('application/json') ?? false;
}

async function parseJSONResponse<T>(response: Response, path: string): Promise<T> {
  if (response.status === 204 || getHeader(response, 'content-length') === '0') {
    return undefined as T;
  }

  if (!isJsonResponse(response)) {
    let bodyPrefix = '';
    try {
      bodyPrefix = (await response.text()).trim().slice(0, 64);
    } catch {
      bodyPrefix = '';
    }

    const contentType = getHeader(response, 'content-type') ?? 'unknown content-type';
    throw new APIError(
      `Expected JSON response from ${path} but received ${contentType}.`,
      response.status,
      'INVALID_RESPONSE',
      false,
      bodyPrefix ? `Response starts with "${bodyPrefix}"` : undefined
    );
  }

  try {
    return (await response.json()) as T;
  } catch {
    throw new APIError(
      `Failed to parse JSON response from ${path}.`,
      response.status,
      'INVALID_RESPONSE'
    );
  }
}

export function createAPIClient(options?: { baseUrl?: string; token?: string }) {
  const baseUrl = options?.baseUrl ?? DEFAULT_BASE_URL;
  // `token` is only for non-browser API clients that explicitly hold a bearer
  // credential (e.g. an integration harness). The browser never supplies one:
  // the HttpOnly session cookie is sent automatically.
  let token = options?.token ?? '';

  async function request<T>(path: string, init?: RequestInit): Promise<T> {
    const headers = new Headers(init?.headers ?? {});
    headers.set('Content-Type', 'application/json');
    if (token) {
      headers.set('Authorization', `Bearer ${token}`);
    }

    let response: Response;
    
    try {
      response = await fetch(`${baseUrl}${path}`, {
        ...init,
        credentials: 'include',
        headers
      });
    } catch (fetchError) {
      // Network error (server unreachable, CORS, etc.)
      const message = getUserFriendlyMessage(fetchError);
      
      // TODO: integrate structured logger instead of console
      // eslint-disable-next-line no-console
      console.error('API Network Error:', {
        path,
        error: fetchError,
        timestamp: new Date().toISOString()
      });
      
      // Show toast for network errors
      toast.error(message);
      
      throw new Error(message);
    }

    if (!response.ok) {
      let payload: APIErrorEnvelope | undefined;
      try {
        if (!isJsonResponse(response)) {
          throw new Error('non-json error response');
        }
        payload = (await response.json()) as APIErrorEnvelope;
      } catch {
        payload = undefined;
      }

      // Handle 401 Unauthorized - clear token and redirect to login
      if (response.status === 401) {
        clearToken();
        
        // Show toast notification
        toast.error('Session expired. Please log in again.');
        
        // Redirect to login page
        if (typeof window !== 'undefined') {
          // Use goto if we're in a Svelte context, otherwise window.location
          try {
            await goto('/login');
          } catch {
            window.location.href = '/login';
          }
        }
        
        throw new APIError(
          'Session expired. Please log in again.',
          401,
          'UNAUTHORIZED',
          false
        );
      }

      // Handle 403 Password Change Required - redirect to change-password
      // The BFF returns a flat object: { code: 'PASSWORD_CHANGE_REQUIRED', ... }
      // Some legacy endpoints may return nested: { error: { code: '...' } }
      const code = payload?.error?.code || (payload as { code?: string } | undefined)?.code;
      if (response.status === 403 && code === 'PASSWORD_CHANGE_REQUIRED') {
        if (typeof window !== 'undefined') {
          window.location.href = '/change-password';
        }
        throw new APIError('Password change required', response.status, 'PASSWORD_CHANGE_REQUIRED');
      }

      // TODO: integrate structured logger instead of console
      // eslint-disable-next-line no-console
      console.error('API Error:', {
        path,
        status: response.status,
        code: payload?.error.code,
        message: payload?.error.message,
        retryable: payload?.error.retryable,
        hint: payload?.error.hint,
        timestamp: new Date().toISOString()
      });

      // Create APIError with full details
      const error = new APIError(
        payload?.error.message ?? `Request failed with status ${response.status}`,
        response.status,
        payload?.error.code ?? 'UNKNOWN_ERROR',
        payload?.error.retryable ?? false,
        payload?.error.hint
      );

      // Show toast for server errors (5xx) and unexpected errors
      if (response.status >= 500) {
        toast.error('A server error occurred. Please try again later.');
      }

      throw error;
    }

    return parseJSONResponse<T>(response, path);
  }


  return {
    /**
     * Set an in-memory bearer token for a non-browser client. Never persists
     * to storage; browser auth uses the HttpOnly cookie instead.
     */
    setToken(next: string) {
      token = next;
    },
    clearToken() {
      token = '';
      clearToken();
    },
    getInstallStatus() {
      return request<InstallStatusResponse>('/api/v1/install/status');
    },
    bootstrapInstall() {
      return request<InstallActionResponse>('/api/v1/install/bootstrap', {
        method: 'POST',
        body: JSON.stringify({})
      });
    },
    repairInstall(body: Record<string, boolean>) {
      return request<InstallActionResponse>('/api/v1/install/repair', {
        method: 'POST',
        body: JSON.stringify(body)
      });
    },
    listVMs() {
      return request<VM[]>('/api/v1/vms');
    },
    listEvents(query = '') {
      return request<Event[]>(`/api/v1/events${query}`);
    },
    logout() {
      return request<void>('/v1/auth/logout', { method: 'POST' });
    },
    listNodes() {
      return request<NodeWithResources[]>('/api/v1/nodes');
    },
    // VM Templates
    listVMTemplates() {
      return request<VMTemplate[]>('/v1/vm-templates');
    },
    createVMTemplate(data: CreateVMTemplateInput) {
      return request<VMTemplate>('/v1/vm-templates', {
        method: 'POST',
        body: JSON.stringify(data)
      });
    },
    cloneFromTemplate(templateId: string, data: CloneFromTemplateInput) {
      return request<VM>(`/v1/vm-templates/${templateId}/clone`, {
        method: 'POST',
        body: JSON.stringify(data)
      });
    },
    // Cloud-init Templates
    listCloudInitTemplates() {
      return request<CloudInitTemplate[]>('/v1/cloud-init-templates');
    },
    createCloudInitTemplate(data: CreateCloudInitTemplateInput) {
      return request<CloudInitTemplate>('/v1/cloud-init-templates', {
        method: 'POST',
        body: JSON.stringify(data)
      });
    },
    renderCloudInitTemplate(templateId: string, data: RenderCloudInitTemplateInput) {
      return request<RenderCloudInitTemplateResponse>(`/v1/cloud-init-templates/${templateId}/render`, {
        method: 'POST',
        body: JSON.stringify(data)
      });
    },
    // Quota endpoints
    createQuota(data: SetQuotaInput) {
      return request<Quota>('/v1/quotas/create', {
        method: 'POST',
        body: JSON.stringify(data)
      });
    },
    updateQuota(userId: string, data: UpdateQuotaInput) {
      return request<Quota>(`/v1/quotas/${userId}`, {
        method: 'PATCH',
        body: JSON.stringify(data)
      });
    },
    getUsage() {
      return request<UsageWithQuota>('/v1/usage', { method: 'POST' });
    }
  };
}
