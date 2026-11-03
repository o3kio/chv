import { browser } from '$app/env';
import { isLoggedIn } from '#lib/api/client.ts';

export interface TaskUpdate {
	task_id: string;
	status: string;
	summary: string;
	resource_kind: string;
	resource_id: string;
	event_unix_ms: number;
}

export class TaskStreamStore {
	private abortCtrl: AbortController | null = null;
	private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
	// Map of task_id -> first-seen timestamp (ms). TTL-bounded so reconnects
	// do not silently drop events that re-arrive after the window expires.
	private seen = new Map<string, number>();
	private pollTimer: ReturnType<typeof setInterval> | null = null;
	status = $state<'idle' | 'connecting' | 'open' | 'error'>('idle');
	onTaskCompleted: ((task: TaskUpdate) => void) | null = null;

	private reconnectDelay = 1000;
	private readonly initialReconnectDelay = 1000;
	private readonly maxReconnectDelay = 30000;
	private readonly SEEN_TTL_MS = 60_000;

	private pruneSeen(): void {
		const cutoff = Date.now() - this.SEEN_TTL_MS;
		for (const [id, ts] of this.seen) {
			if (ts < cutoff) this.seen.delete(id);
		}
	}

	startPollingFallback(intervalMs = 10_000) {
		this.stopPollingFallback();
		this.pollTimer = setInterval(async () => {
			try {
				if (!isLoggedIn()) return;
				const res = await fetch('/v1/tasks', {
					method: 'POST',
					credentials: 'include',
					headers: {
						'Content-Type': 'application/json',
					},
					body: JSON.stringify({ page: 1, page_size: 50 }),
				});
				if (res.status === 401) {
					// Token invalid/expired — match SSE semantics: stop and surface error.
					this.status = 'error';
					this.stopPollingFallback();
					return;
				}
				if (!res.ok) return;
				const data = await res.json();
				const terminal = ['Completed', 'Failed', 'Cancelled'];
				this.pruneSeen();
				for (const item of data.items || []) {
					if (!terminal.includes(item.status)) continue;
					if (this.seen.has(item.task_id)) continue;
					this.seen.set(item.task_id, Date.now());
					try {
						this.onTaskCompleted?.(item);
					} catch (cbErr) {
						// TODO: integrate structured logger instead of console
						// eslint-disable-next-line no-console
						console.error('[taskStream] onTaskCompleted handler threw (polling):', cbErr);
					}
				}
			} catch (err) {
				// TODO: integrate structured logger instead of console
				// eslint-disable-next-line no-console
				console.warn('[taskStream] polling fallback request failed:', err);
			}
		}, intervalMs);
	}

	stopPollingFallback() {
		if (this.pollTimer) {
			clearInterval(this.pollTimer);
			this.pollTimer = null;
		}
	}

	private scheduleReconnect(resourceKinds?: string[]) {
		if (this.reconnectTimer) return;
		this.reconnectTimer = setTimeout(() => {
			this.reconnectTimer = null;
			this.connect(resourceKinds);
		}, this.reconnectDelay);
		this.reconnectDelay = Math.min(this.reconnectDelay * 2, this.maxReconnectDelay);
	}

	async connect(resourceKinds?: string[]) {
		if (!browser) return;
		this.disconnect();
		if (!isLoggedIn()) {
			this.startPollingFallback();
			return;
		}

		const url = new URL('/v1/tasks/stream', window.location.origin);
		if (resourceKinds && resourceKinds.length > 0) {
			url.searchParams.set('resource_kinds', resourceKinds.join(','));
		}

		this.abortCtrl = new AbortController();
		this.status = 'connecting';

		try {
			const res = await fetch(url.toString(), {
				credentials: 'include',
				headers: {
					Accept: 'text/event-stream',
				},
				signal: this.abortCtrl.signal,
			});

			if (!res.ok) {
				if (res.status === 401) {
					this.status = 'error';
					// Token invalid/expired — don't retry, let user re-login
					return;
				}
				throw new Error(`HTTP ${res.status}`);
			}

			if (!res.body) {
				throw new Error('No response body');
			}

			this.status = 'open';
			this.reconnectDelay = this.initialReconnectDelay; // Reset on successful connection

			const reader = res.body.getReader();
			const decoder = new TextDecoder();
			let buffer = '';

			while (true) {
				const { done, value } = await reader.read();
				if (done) break;

				buffer += decoder.decode(value, { stream: true });
				const lines = buffer.split('\n');
				buffer = lines.pop() ?? '';

				let dataLine = '';
				for (const line of lines) {
					if (line.startsWith('data:')) {
						dataLine = line.slice(5).trim();
					} else if (line === '' && dataLine) {
						// Parse JSON in its own scope so handler exceptions are not
						// silently swallowed by the malformed-message catch.
						let payload: { items?: TaskUpdate[] } | null = null;
						try {
							payload = JSON.parse(dataLine) as { items?: TaskUpdate[] };
						} catch (parseErr) {
							// TODO: integrate structured logger instead of console
							// eslint-disable-next-line no-console
							console.warn('[taskStream] dropping malformed SSE message:', parseErr);
						}
						if (payload) {
							const items: TaskUpdate[] = payload.items ?? [];
							const terminal = ['Completed', 'Failed', 'Cancelled'];
							this.pruneSeen();
							for (const task of items) {
								if (!terminal.includes(task.status)) continue;
								if (this.seen.has(task.task_id)) continue;
								this.seen.set(task.task_id, Date.now());
								try {
									this.onTaskCompleted?.(task);
								} catch (cbErr) {
									// TODO: integrate structured logger instead of console
									// eslint-disable-next-line no-console
									console.error(
										'[taskStream] onTaskCompleted handler threw (SSE):',
										cbErr
									);
								}
							}
						}
						dataLine = '';
					}
				}
			}

			// Stream ended normally — reconnect
			this.status = 'idle';
			this.scheduleReconnect(resourceKinds);
		} catch (err) {
			if ((err as Error).name === 'AbortError') {
				this.status = 'idle';
				return;
			}
			this.status = 'error';
			this.scheduleReconnect(resourceKinds);
		}
	}

	disconnect() {
		// Reset reconnect backoff and dedupe state first so a subsequent
		// connect() starts from a clean slate.
		this.reconnectDelay = this.initialReconnectDelay;
		this.seen.clear();
		this.stopPollingFallback();
		if (this.reconnectTimer) {
			clearTimeout(this.reconnectTimer);
			this.reconnectTimer = null;
		}
		if (this.abortCtrl) {
			this.abortCtrl.abort();
			this.abortCtrl = null;
		}
		this.status = 'idle';
	}
}

export const taskStream = new TaskStreamStore();
