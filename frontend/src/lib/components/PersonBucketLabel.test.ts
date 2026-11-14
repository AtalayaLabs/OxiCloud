import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/svelte';

const resolveUser = vi.hoisted(() => vi.fn());

vi.mock('$lib/api/endpoints/users', () => ({ resolveUser }));

import PersonBucketLabel from './PersonBucketLabel.svelte';

// The "no person, no fetch" assertion below counts calls, so the count has to
// start at zero rather than carrying the previous test's.
beforeEach(() => {
	vi.clearAllMocks();
});

it('renders the avatar beside the name on a person bucket', async () => {
	resolveUser.mockResolvedValue({ name: 'Bob Stone', email: 'bob@example.com', image: null });

	render(PersonBucketLabel, { userId: 'u1', label: 'Bob Stone' });

	expect(screen.getByText('Bob Stone')).toBeTruthy();
	// Initials, because the resolved user has no photo — the avatar is there
	// either way, and `BS` is what proves it rendered rather than a gap.
	await waitFor(() => expect(screen.getByText('BS')).toBeTruthy());
	expect(resolveUser).toHaveBeenCalledWith('u1');
});

// THE reason `userId` is passed conditionally by every caller instead of just
// being the bucket key: on a date or type axis the key is "Today" or "Image",
// and an avatar there would fetch `/api/users/Today` and then draw a coloured
// "T" circle as though a person called Today existed.
it('renders no avatar — and fetches no user — without a person', async () => {
	render(PersonBucketLabel, { userId: null, label: 'Today' });

	expect(screen.getByText('Today')).toBeTruthy();
	expect(resolveUser).not.toHaveBeenCalled();
});
