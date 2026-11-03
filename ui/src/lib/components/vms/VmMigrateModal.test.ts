import { render, screen, cleanup, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, expect, it, afterEach, vi } from 'vitest';
import VmMigrateModal from './VmMigrateModal.svelte';

// The modal fetches eligible nodes on open — stub the BFF hop.
vi.mock('#lib/bff/nodes.ts', () => ({
	listNodes: vi.fn().mockResolvedValue({
		items: [
			{
				node_id: 'node-b',
				name: 'node-b',
				cluster: 'c1',
				state: 'TenantReady',
				health: 'healthy',
				cpu: '2/8',
				memory: '4/32 GiB',
				storage: '10/100 GiB',
				network: 'ok',
				version: '0.3.0'
			}
		],
		page: { page: 1, page_size: 100, total_items: 1 },
		filters: { applied: {} }
	})
}));

vi.mock('#lib/api/client.ts', () => ({
	getStoredToken: vi.fn().mockReturnValue('test-token')
}));

describe('VmMigrateModal', () => {
	afterEach(() => {
		cleanup();
	});

	function renderModal() {
		const onmigrate = vi.fn();
		const onclose = vi.fn();
		const { rerender } = render(VmMigrateModal, {
			props: {
				open: true,
				vmId: 'vm-test',
				currentNodeId: 'node-a',
				submitting: false,
				onmigrate,
				onclose
			}
		});
		return { onmigrate, onclose, rerender };
	}

	async function waitForNodes() {
		await waitFor(() => {
			expect(screen.getByRole('option', { name: /node-b/i })).toBeTruthy();
		});
	}

	function pauseFirstCheckbox() {
		return screen.getByLabelText(/pause-first \(stop-the-world/i) as HTMLInputElement;
	}

	it('defaults to pause-first: the control is checked, disabled, and the live path is called out', async () => {
		renderModal();
		await waitForNodes();
		const checkbox = pauseFirstCheckbox();
		expect(checkbox.checked).toBe(true);
		expect(checkbox.disabled).toBe(true);
		expect(screen.getByText(/Live migration is disabled until #394 is proven/i)).toBeTruthy();
		expect(screen.getByText(/stays paused for the full disk and memory transfer/i)).toBeTruthy();
		expect(screen.getByText(/unreachable for the entire migration/i)).toBeTruthy();
		expect(screen.queryByText(/will remain running during the migration/i)).toBeNull();
	});

	it('always sends pauseFirst=true: no live path is reachable', async () => {
		const { onmigrate } = renderModal();
		await waitForNodes();
		fireEvent.change(screen.getByLabelText(/target node/i), { target: { value: 'node-b' } });
		fireEvent.click(screen.getByRole('button', { name: /^Migrate$/i }));
		expect(onmigrate).toHaveBeenCalledWith('node-b', true);
		expect(onmigrate).not.toHaveBeenCalledWith('node-b', false);
	});

	it('keeps pause-first mandatory when the modal reopens', async () => {
		const { rerender } = renderModal();
		await waitForNodes();
		expect(pauseFirstCheckbox().checked).toBe(true);
		// Close (open=false unmounts the form body), then reopen: the
		// mandatory mode must survive as checked, not reset to live.
		await rerender({ open: false });
		await rerender({ open: true });
		await waitForNodes();
		expect(pauseFirstCheckbox().checked).toBe(true);
		expect(pauseFirstCheckbox().disabled).toBe(true);
		expect(screen.getByText(/Live migration is disabled until #394 is proven/i)).toBeTruthy();
	});
});
