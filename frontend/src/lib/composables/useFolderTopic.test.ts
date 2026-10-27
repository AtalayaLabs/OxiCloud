// Regression pin for the mount-folder subscribe-denial surfaced by
// #837 (see 2026-10-26 fix): mount folders have no server-side
// publishers, so subscribing to `folder:ext:...` turns into a
// server-rejected topic → `onRevoked` fires → the folder view
// toasts "access revoked" + redirects to /files on every mount
// subfolder open.
//
// `folderTopicFor` is the pure helper that gates `useFolderTopic`'s
// subscribe. The reactive glue around it isn't tested here (would
// need a full Svelte 5 harness); the branch condition is what a
// future regression would collapse, and that's exactly what this
// file pins.
//
// If a future change ever makes mount folders publishable (the
// server would also have to teach `Topic::parse` + `bus.publish`
// about `ext:` ids), this test should flip: `ext:...` would get a
// non-null topic and the subscribe would succeed.

import { describe, it, expect } from 'vitest';
import { folderTopicFor } from './useFolderTopic.svelte';

describe('folderTopicFor', () => {
	it('maps a native UUID to its folder topic', () => {
		const uuid = '00000000-0000-0000-0000-000000000001';
		expect(folderTopicFor(uuid)).toBe(`folder:${uuid}`);
	});

	it('returns null for null / undefined / empty — no subscribe', () => {
		expect(folderTopicFor(null)).toBeNull();
		expect(folderTopicFor(undefined)).toBeNull();
		expect(folderTopicFor('')).toBeNull();
	});

	// The regression pin.
	//
	// If someone ever drops the `ext:`-skip branch (looks dead to
	// a careless reader because no production code sets ext: topics
	// as publishable server-side), this test fails. Removing it
	// would silently reintroduce the "access revoked" boot on every
	// mount subfolder navigation.
	it('returns null for ext: ids — mount folders have no publishers', () => {
		// Shape produced by `encode_child_id(mount_uuid, "images")`
		// — the mount envelope, not that the token is base64-decodable.
		const extId = 'ext:f48ef5e528ed4b2691d34cedec846e33:aW1hZ2Vz';
		expect(folderTopicFor(extId)).toBeNull();
	});

	it('returns null for the one-segment ext: shape too', () => {
		// Covers a mount child whose node_id is a single segment
		// (vs. the nested-leaf shape above with a longer path).
		const extId = 'ext:f48ef5e528ed4b2691d34cedec846e33:bW91bnRfcm9vdA';
		expect(folderTopicFor(extId)).toBeNull();
	});
});
