<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { Terminal } from 'xterm';
	import { FitAddon } from 'xterm-addon-fit';
	import 'xterm/css/xterm.css';
	import { PlugZap, Unplug, Copy, Check, Download } from 'lucide-svelte';

	interface Props {
		vmId: string;
		consoleUrl: string;
		/**
		 * Short-lived console ticket issued by the BFF. Passed as the
		 * WebSocket subprotocol (#660), never appended to the URL.
		 */
		consoleToken?: string;
		getConsoleUrl?: () => Promise<ConsoleConnection>;
		running?: boolean;
		consoleExpiresAt?: string;
	}

	interface ConsoleConnection {
		url: string;
		token: string;
	}

	let {
		vmId,
		consoleUrl,
		consoleToken,
		getConsoleUrl,
		running = true,
		consoleExpiresAt
	}: Props = $props();

	let terminalEl: HTMLDivElement;
	let terminal: Terminal;
	let fitAddon: FitAddon;
	let socket: WebSocket | undefined;
	let reconnectTimer: ReturnType<typeof setTimeout> | undefined;
	let resizeObserver: ResizeObserver | undefined;
	let activeSocketUrl = '';
	let connected = $state(false);
	let statusText = $state('Disconnected');
	let copied = $state(false);
	let copyTimer: ReturnType<typeof setTimeout>;
	let wsError = $state('');
	let terminalReady = $state(false);
	let manualDisconnect = $state(false);
	let reconnecting = false;
	let reconnectDelay = 1500;
	const MAX_RECONNECT_DELAY = 30000;
	let tokenRefreshFailures = 0;
	const MAX_TOKEN_REFRESH_FAILURES = 3;

	function validateWsUrl(url: string): boolean {
		// Allow relative paths (starting with /) — they're always same-origin
		if (url.startsWith('/')) return true;
		// Allow ws:// or wss:// only if they target the same host
		if (url.startsWith('ws://') || url.startsWith('wss://')) {
			try {
				const parsed = new URL(url);
				return parsed.host === window.location.host;
			} catch {
				return false;
			}
		}
		return false;
	}

	function buildWsUrl(url: string): string {
		return url.startsWith('ws')
			? url
			: `${window.location.protocol === 'https:' ? 'wss:' : 'ws:'}//${window.location.host}${url}`;
	}

	function clearReconnectTimer() {
		if (reconnectTimer) {
			clearTimeout(reconnectTimer);
			reconnectTimer = undefined;
		}
	}

	function scheduleReconnect() {
		if (manualDisconnect || !running || !consoleUrl || !terminalReady) return;
		clearReconnectTimer();
		statusText = 'Reconnecting';
		reconnectTimer = setTimeout(() => {
			handleReconnect(false);
		}, reconnectDelay);
		reconnectDelay = Math.min(reconnectDelay * 2, MAX_RECONNECT_DELAY);
	}

	function connectWith(url: string, token: string | undefined) {
		if (!terminal) return;
		if (socket?.readyState === WebSocket.CONNECTING) return;
		if (!token) {
			wsError = 'Console ticket missing. Please reconnect.';
			statusText = 'Token missing';
			terminal.writeln('\r\n\x1b[31m[Console ticket missing — please reconnect]\x1b[0m');
			return;
		}
		if (!validateWsUrl(url)) {
			wsError = `Refused to connect: WebSocket URL does not match the application origin.`;
			statusText = 'Connection blocked';
			terminal.writeln('\r\n\x1b[31m[Connection blocked: invalid WebSocket origin]\x1b[0m');
			return;
		}
		wsError = '';
		clearReconnectTimer();
		const wsUrl = buildWsUrl(url);
		if (
			socket &&
			activeSocketUrl === wsUrl &&
			// `CONNECTING` is unreachable here: connectWith returns early above
			// when the existing socket is still connecting.
			socket.readyState === WebSocket.OPEN
		) {
			return;
		}

		if (socket) {
			socket.onopen = null;
			socket.onmessage = null;
			socket.onerror = null;
			socket.onclose = null;
			socket.close();
			// Clear terminal on reconnect so scrollback isn't duplicated
			terminal.clear();
		}
		activeSocketUrl = wsUrl;
		statusText = 'Connecting';
		// The ticket travels as the WebSocket subprotocol, not in the URL
		// (#660), so it never reaches access logs or browser history.
		socket = new WebSocket(wsUrl, [token]);
		socket.binaryType = 'arraybuffer';

		socket.onopen = () => {
			connected = true;
			statusText = 'Connected';
			reconnectDelay = 1500;
			tokenRefreshFailures = 0;
			terminal.writeln('\r\n\x1b[32m[Connected to serial console]\x1b[0m\r\n');
		};

		socket.onmessage = (event) => {
			if (event.data instanceof ArrayBuffer) {
				const data = new Uint8Array(event.data);
				terminal.write(data);
			} else if (typeof event.data === 'string') {
				terminal.write(event.data);
			}
		};

		socket.onclose = () => {
			connected = false;
			statusText = 'Disconnected';
			terminal.writeln('\r\n\x1b[31m[Disconnected]\x1b[0m');
			scheduleReconnect();
		};

		socket.onerror = () => {
			connected = false;
			statusText = 'Connection error';
			terminal.writeln('\r\n\x1b[31m[Connection error]\x1b[0m');
		};
	}

	async function handleReconnect(userInitiated = true) {
		if (reconnecting) return;
		reconnecting = true;
		try {
			if (userInitiated) {
				manualDisconnect = false;
				reconnectDelay = 1500;
				tokenRefreshFailures = 0;
			}
			let urlToUse = consoleUrl;
			let tokenToUse = consoleToken;

			if (getConsoleUrl) {
				statusText = 'Refreshing token...';
				try {
					const connection = await getConsoleUrl();
					urlToUse = connection.url;
					tokenToUse = connection.token;
					tokenRefreshFailures = 0;
				} catch {
					tokenRefreshFailures++;
					const isExpired = consoleExpiresAt && Date.now() > new Date(consoleExpiresAt).getTime() - 10000;
					if (isExpired || tokenRefreshFailures >= MAX_TOKEN_REFRESH_FAILURES) {
						wsError = 'Console token expired. Please reconnect manually.';
						statusText = 'Token expired';
						terminal.writeln('\r\n\x1b[31m[Console token expired — please reconnect manually]\x1b[0m');
						return;
					}
					terminal.writeln('\r\n\x1b[33m[Token refresh failed, will retry]\x1b[0m');
					scheduleReconnect();
					return;
				}
			}

			connectWith(urlToUse, tokenToUse);
		} finally {
			reconnecting = false;
		}
	}

	function handleDisconnect() {
		manualDisconnect = true;
		clearReconnectTimer();
		if (socket) {
			socket.onclose = () => {
				connected = false;
				statusText = 'Disconnected';
				terminal.writeln('\r\n\x1b[31m[Disconnected]\x1b[0m');
			};
			socket.close();
		}
	}

	function getTerminalContent(): string {
		const buffer = terminal.buffer.active;
		const lines: string[] = [];
		for (let i = 0; i < buffer.length; i++) {
			const line = buffer.getLine(i);
			if (line) lines.push(line.translateToString(true));
		}
		return lines.join('\n');
	}

	async function handleCopy() {
		const text = getTerminalContent();
		try {
			await navigator.clipboard.writeText(text);
			copied = true;
			clearTimeout(copyTimer);
			copyTimer = setTimeout(() => {
				copied = false;
			}, 2000);
		} catch {
			terminal.writeln('\r\n\x1b[33m[Copy failed: clipboard not available]\x1b[0m');
		}
	}

	function handleDownload() {
		const text = getTerminalContent();
		const timestamp = new Date().toISOString().replace(/[:.]/g, '-');
		const filename = `console-${vmId}-${timestamp}.txt`;
		const blob = new Blob([text], { type: 'text/plain' });
		const url = URL.createObjectURL(blob);
		const a = document.createElement('a');
		a.href = url;
		a.download = filename;
		a.click();
		URL.revokeObjectURL(url);
	}

	function showNotRunning() {
		terminal.clear();
		terminal.write('\r\n');
		terminal.writeln('\x1b[33m╔════════════════════════════════════════╗\x1b[0m');
		terminal.writeln('\x1b[33m║                                        ║\x1b[0m');
		terminal.writeln('\x1b[33m║\x1b[0m  \x1b[1mInstance is not running\x1b[0m             \x1b[33m║\x1b[0m');
		terminal.writeln('\x1b[33m║\x1b[0m  Start the VM to access the console.  \x1b[33m║\x1b[0m');
		terminal.writeln('\x1b[33m║                                        ║\x1b[0m');
		terminal.writeln('\x1b[33m╚════════════════════════════════════════╝\x1b[0m');
		terminal.write('\r\n');
	}

	$effect(() => {
		if (!terminalReady) return;
		if (running && consoleUrl && !manualDisconnect) {
			connectWith(consoleUrl, consoleToken);
		} else {
			if (socket) {
				socket.onopen = null;
				socket.onmessage = null;
				socket.onerror = null;
				socket.onclose = null;
				socket.close();
				socket = undefined;
			}
			clearReconnectTimer();
			connected = false;
			statusText = running ? 'Disconnected' : 'Instance not running';
			if (!running) showNotRunning();
		}
	});

	function safeFit() {
		if (!terminalEl || !fitAddon) return;
		// offsetParent is null when the element (or an ancestor) is display:none.
		// Calling fit() in that state crashes xterm.js because dimensions are undefined.
		if (terminalEl.offsetParent === null) return;
		try {
			fitAddon.fit();
		} catch (e) {
			// Silently ignore transient dimension errors
		}
	}

	onMount(() => {
		const rootStyle = getComputedStyle(document.documentElement);
		terminal = new Terminal({
			cursorBlink: true,
			fontSize: 13,
			fontFamily: rootStyle.getPropertyValue('--font-mono').trim() || 'monospace',
			allowProposedApi: true,
			scrollback: 10000,
			theme: {
				background: '#000000',
				foreground: '#e0e0e0',
				cursor: rootStyle.getPropertyValue('--shell-accent').trim() || '#8f5a2a',
				selectionBackground: 'rgba(143, 90, 42, 0.22)'
			}
		});
		fitAddon = new FitAddon();
		terminal.loadAddon(fitAddon);
		terminal.open(terminalEl);
		safeFit();

		terminal.onData((data) => {
			if (socket?.readyState === WebSocket.OPEN) {
				socket.send(data);
			}
		});

		terminal.onResize(({ cols, rows }) => {
			if (socket?.readyState === WebSocket.OPEN) {
				socket.send(JSON.stringify({ type: 'resize', cols, rows }));
			}
		});

		resizeObserver = new ResizeObserver(() => {
			safeFit();
		});
		resizeObserver.observe(terminalEl);

		if (!running) {
			showNotRunning();
		}
		terminalReady = true;
	});

	onDestroy(() => {
		clearTimeout(copyTimer);
		clearReconnectTimer();
		resizeObserver?.disconnect();
		socket?.close();
		terminal?.dispose();
	});
</script>

<div class="console-wrapper">
	{#if wsError}
		<div class="console-error">{wsError}</div>
	{/if}
	<div class="console-toolbar">
		<div class="toolbar-left">
			<span class="console-status">
				<span class="status-dot" class:connected></span>
				{statusText}
			</span>
			<span class="console-meta">VM {vmId}</span>
		</div>
		<div class="toolbar-right">
			<button
				type="button"
				class="toolbar-btn"
				title="Copy terminal contents"
				aria-label={copied ? 'Terminal contents copied' : 'Copy terminal contents'}
				onclick={handleCopy}
			>
				{#if copied}
					<Check size={14} />
				{:else}
					<Copy size={14} />
				{/if}
			</button>
			<button
				type="button"
				class="toolbar-btn"
				title="Download terminal contents"
				aria-label="Download terminal contents"
				onclick={handleDownload}
			>
				<Download size={14} />
			</button>
			{#if statusText !== 'Refreshing token...'}
				{#if connected}
					<button type="button"
						class="toolbar-btn"
						title="Disconnect"
						aria-label="Disconnect from console"
						onclick={handleDisconnect}
					>
						<Unplug size={14} />
					</button>
				{:else}
					<button type="button"
						class="toolbar-btn"
						title="Reconnect"
						aria-label="Reconnect to console"
						onclick={() => handleReconnect()}
					>
						<PlugZap size={14} />
					</button>
				{/if}
			{/if}
		</div>
	</div>
	<div bind:this={terminalEl} class="terminal-container"></div>
</div>

<style>
	.console-wrapper {
		width: 100%;
		background: var(--shell-surface);
		border-radius: var(--radius-sm);
		border: 1px solid var(--shell-line);
		overflow: hidden;
	}

	.console-toolbar {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 0.45rem 0.65rem;
		background: var(--shell-surface-muted);
		border-bottom: 1px solid var(--shell-line);
		font-size: var(--text-xs);
	}

	.toolbar-left {
		display: flex;
		align-items: center;
		gap: 0.6rem;
	}

	.toolbar-right {
		display: flex;
		align-items: center;
		gap: 0.2rem;
	}

	.console-status {
		display: flex;
		align-items: center;
		gap: 0.4rem;
		color: var(--shell-text-secondary);
	}

	.status-dot {
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--color-danger);
	}

	.status-dot.connected {
		background: var(--color-success);
	}

	.console-meta {
		color: var(--shell-text-muted);
		font-family: var(--font-mono);
	}

	.toolbar-btn {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 1.75rem;
		height: 1.75rem;
		padding: 0;
		background: transparent;
		border: 1px solid transparent;
		border-radius: var(--radius-xs);
		color: var(--shell-text-secondary);
		cursor: pointer;
		transition:
			background 120ms ease,
			border-color 120ms ease,
			color 120ms ease;
	}

	.toolbar-btn:hover {
		background: var(--shell-accent-soft);
		border-color: var(--shell-accent);
		color: var(--shell-text);
	}

	.terminal-container {
		width: 100%;
		height: min(54vh, 34rem);
		min-height: 22rem;
		padding: 0.5rem;
		background: #000000;
	}

	:global(.xterm) {
		height: 100%;
	}

	:global(.xterm-screen) {
		background: transparent !important;
	}

	:global(.xterm-viewport) {
		background: transparent !important;
	}

	.console-error {
		padding: 0.5rem 0.75rem;
		background: var(--color-danger-light);
		border-bottom: 1px solid var(--color-danger);
		color: var(--color-danger-dark);
		font-size: var(--text-xs);
	}
</style>
