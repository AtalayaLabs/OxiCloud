import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

const { pageState } = vi.hoisted(() => ({
	pageState: { url: new URL('http://localhost/device') }
}));
vi.mock('$app/state', () => ({ page: pageState }));

// Phase 4: the handwritten `endpoints/device.ts` wrapper is retired;
// the page calls the generated SDK operations directly. Mock the SDK
// surface so these tests stay unit-level (no real network).
vi.mock('$lib/api/generated/openapi/sdk.gen', () => ({
	deviceVerifyInfo: vi.fn(),
	deviceVerifyAction: vi.fn()
}));

import { deviceVerifyAction, deviceVerifyInfo } from '$lib/api/generated/openapi/sdk.gen';
import DevicePage from './+page.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;
// The SDK returns the parsed body directly (generator-level
// `responseStyle: 'data'`), so the mock resolves with the shape the
// Rust `DeviceVerifyInfoDto` emits — `valid: true` + metadata.
const info = { valid: true, client_name: 'CLI', scopes: 'files' };

beforeEach(() => {
	vi.clearAllMocks();
	pageState.url = new URL('http://localhost/device');
});

it('looks up a typed code and shows the review step', async () => {
	m(deviceVerifyInfo).mockResolvedValue(info);
	render(DevicePage);
	await screen.findByTestId('device-code-form');
	await fireEvent.input(screen.getByTestId('device-code-input'), {
		target: { value: 'WXYZ-1234' }
	});
	await fireEvent.click(screen.getByTestId('device-continue-btn'));
	await waitFor(() =>
		expect(deviceVerifyInfo).toHaveBeenCalledWith(
			expect.objectContaining({ query: { code: 'WXYZ-1234' } })
		)
	);
	await screen.findByTestId('device-approve-btn');
});

it('approves a device after lookup', async () => {
	m(deviceVerifyInfo).mockResolvedValue(info);
	m(deviceVerifyAction).mockResolvedValue({ status: 'approved' });
	render(DevicePage);
	await fireEvent.input(await screen.findByTestId('device-code-input'), {
		target: { value: 'WXYZ-1234' }
	});
	await fireEvent.click(screen.getByTestId('device-continue-btn'));
	await fireEvent.click(await screen.findByTestId('device-approve-btn'));
	await waitFor(() => expect(deviceVerifyAction).toHaveBeenCalled());
});

it('shows an error when lookup fails', async () => {
	m(deviceVerifyInfo).mockRejectedValue(new Error('nope'));
	render(DevicePage);
	await fireEvent.input(await screen.findByTestId('device-code-input'), {
		target: { value: 'BAD' }
	});
	await fireEvent.click(screen.getByTestId('device-continue-btn'));
	await screen.findByTestId('device-retry-btn');
});
