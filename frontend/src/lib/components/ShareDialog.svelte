<script lang="ts">
	import { errorToast } from '$lib/utils/errors';
	import {
		copyShareLink,
		createShare,
		deleteShare,
		listSharesForItem,
		updateShare
	} from '$lib/api/endpoints/shares';
	import {
		createGrant,
		expiryToIso,
		displayRole,
		fetchGrantsForResource,
		notifyGrantRecipient,
		revokeGrant,
		todayIso,
		updateGrantRole,
		type Grant,
		type GrantSubject,
		type GrantSubjectInput,
		type NotifyOutcome,
		type ShareRole
	} from '$lib/api/endpoints/grants';
	import {
		ensureResolvers,
		isDirectoryAvailable,
		resolveRecipient,
		searchRecipients,
		type Recipient
	} from '$lib/api/endpoints/recipients';
	import { getFolderAncestorsWithGrants } from '$lib/api/endpoints/folders';
	import { getFile } from '$lib/api/endpoints/files';
	import { resolve } from '$app/paths';
	import type { ShareItem } from '$lib/api/types';
	import type { GrantResourceType } from '$lib/api/endpoints/grants';
	import Icon from '$lib/icons/Icon.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import UserVignette from '$lib/components/UserVignette.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { ui } from '$lib/stores/ui.svelte';
	import { drives as drivesStore } from '$lib/stores/drives.svelte';
	import { readAllPolicies } from '$lib/utils/drivePolicies';

	interface Target {
		id: string;
		name: string;
		kind: GrantResourceType;
	}

	interface Props {
		open: boolean;
		item: Target | null;
		/** Fired with the item id when an outgoing share (grant or link) is created. */
		onshared?: (id: string) => void;
		/**
		 * Fired with the item id on **any** membership mutation — create,
		 * role change, expiry change, removal, or public-link creation.
		 * Distinct from `onshared` because some callers (file/folder list
		 * views that toggle a "shared" badge) only care about creation;
		 * the drive-config view needs to refresh on every change.
		 */
		onchange?: (id: string) => void;
		/**
		 * Whether the "Public link" (token-grant) tab is exposed. Defaults to
		 * `true` for file/folder sharing. Drives set this to `false`: a drive
		 * grant is per-member only, never via a shareable URL — exposing the
		 * tab would suggest a capability that doesn't exist.
		 */
		allowLinks?: boolean;
		/**
		 * Re-point the dialog at another resource, without closing it.
		 *
		 * Only the source chip on an inherited row uses this: that chip
		 * also navigates (its `href` moves the page behind the dialog to
		 * the source folder), and on `/files` that navigation keeps the
		 * SAME route — so the page never remounts and `item` would stay
		 * pinned to the folder the user started from. The dialog would
		 * sit there still showing the inherited, greyed rows, which is
		 * exactly the wrong half of the point: the user clicked to go
		 * somewhere they can EDIT the grant.
		 *
		 * Pages that own a share target wire this to reassign it; the
		 * load effect keys off `item`, so re-pointing reloads by itself.
		 * Optional because pages reached from a different route unmount
		 * on navigation anyway and have nothing stale to fix.
		 */
		onretarget?: (target: Target) => void;
	}

	let {
		open = $bindable(false),
		item,
		onshared,
		onchange,
		allowLinks = true,
		onretarget
	}: Props = $props();

	// When the public-link tab is hidden, force the People view — otherwise a
	// caller toggling `allowLinks` between renders could land on the now-hidden
	// tab with no UI.
	let tab = $state<'people' | 'link'>('people');
	$effect(() => {
		if (!allowLinks) tab = 'people';
	});
	let directoryAvailable = $state(true);

	const ROLES: { v: ShareRole; l: string; icon: string }[] = [
		{ v: 'owner', l: t('share.role.canManage', 'Can manage'), icon: 'crown' },
		{ v: 'editor', l: t('share.role.canEdit', 'Can edit'), icon: 'pencil-alt' },
		{ v: 'viewer', l: t('share.role.canView', 'Can view'), icon: 'eye' }
	];
	const ROLE_ORDER: ShareRole[] = ['owner', 'editor', 'viewer'];
	function roleLabel(r: ShareRole): string {
		return ROLES.find((x) => x.v === r)?.l ?? r;
	}
	function roleIcon(r: ShareRole): string {
		return ROLES.find((x) => x.v === r)?.icon ?? 'eye';
	}

	// ── People / grants ──────────────────────────────────────────────────────
	interface Member {
		subject: GrantSubject;
		recipient: Recipient;
		role: ShareRole;
		grantIds: string[];
		/** Representative grant id for notify (any grant on this subject). */
		notifyGrantId?: string;
		expiry: string | null; // YYYY-MM-DD or null
		/**
		 * Set when this access comes from somewhere ABOVE the item — an
		 * ancestor folder or the drive — rather than from a grant on the
		 * item itself. Undefined means direct.
		 *
		 * Inherited rows are read-only here: the grant lives on another
		 * resource, so editing it from this dialog would silently mutate
		 * a different folder's sharing. The row links to the source
		 * instead.
		 */
		inherited?: {
			/** Folder or drive name, for the source chip. */
			label: string;
			/**
			 * Where the chip goes. Exactly one is set.
			 *
			 * `folderId` → `/files/{id}`, where the grant is direct and
			 * this same dialog can edit it — so that case also re-points
			 * the dialog. `driveId` → `/config/drive/{uuid}`, which runs
			 * the drive-kind dialog instead; navigation only, because
			 * drive membership is a different grant type with its own
			 * role vocabulary and no public links.
			 */
			folderId: string | null;
			driveId: string | null;
		};
	}
	/**
	 * A public link on an ancestor folder (or the drive) that also
	 * reaches this item.
	 *
	 * Deliberately thinner than {@link ShareItem}: the effective-grants
	 * walk returns the token GRANT, not the share row, so there is no
	 * link name, password flag or URL here. That is the honest shape —
	 * this section exists to say "this is publicly reachable, and here is
	 * where that was decided", and the chip goes to the folder that owns
	 * the link so it can be inspected or revoked at its source.
	 */
	interface InheritedLink {
		grantId: string;
		/** The link's own name, as its creator set it. */
		name: string;
		hasPassword: boolean;
		/** Source folder or drive name. */
		label: string;
		/** See `Member.inherited` — exactly one of these is set. */
		folderId: string | null;
		driveId: string | null;
		expiry: string | null;
	}

	let members = $state<Member[]>([]);
	let inheritedLinks = $state<InheritedLink[]>([]);
	let grantsLoading = $state(false);
	let query = $state('');
	let results = $state<Recipient[]>([]);
	let newRole = $state<ShareRole>('viewer');
	let newExpiry = $state<string | null>(null);
	/**
	 * The recipient chosen from the search results, not yet granted.
	 *
	 * Picking a result used to create the grant outright, which meant the
	 * role and expiry beside the search box only had any effect if the user
	 * had set them BEFORE typing a name — so in practice everyone was added
	 * as the default role and corrected afterwards. Holding the choice here
	 * lets the role be decided first, and makes [Add] the moment of commit.
	 */
	let selected = $state<Recipient | null>(null);
	let adding = $state(false);
	let searchTimer: ReturnType<typeof setTimeout> | null = null;

	function isoToDate(iso: string | null | undefined): string | null {
		return iso ? String(iso).slice(0, 10) : null;
	}

	function groupGrants(grants: Grant[]): Member[] {
		// Transient scratch map used to fold grants into Member rows, then discarded.
		// eslint-disable-next-line svelte/prefer-svelte-reactivity
		const bySubject = new Map<
			string,
			{ subject: GrantSubject; role: ShareRole; ids: string[]; expiry: string | null }
		>();
		for (const g of grants) {
			if (g.subject.type === 'token') continue;
			const key = `${g.subject.type}:${g.subject.id}`;
			const entry = bySubject.get(key) ?? {
				subject: g.subject,
				role: 'viewer' as ShareRole,
				ids: [],
				expiry: null
			};
			// Role-grants emit one row per (subject, resource), so the row's role
			// is the subject's role directly.
			entry.role = displayRole(g.role);
			entry.ids.push(g.id);
			if (g.expires_at && !entry.expiry) entry.expiry = isoToDate(g.expires_at);
			bySubject.set(key, entry);
		}
		return [...bySubject.values()].map((e) => ({
			subject: e.subject,
			recipient: resolveRecipient(e.subject.type as 'user' | 'group', e.subject.id),
			role: e.role,
			grantIds: e.ids,
			notifyGrantId: e.ids[0],
			expiry: e.expiry
		}));
	}

	/**
	 * Inherited access: grants sitting on an ancestor folder or the
	 * drive, which reach this item by cascade.
	 *
	 * `walkFrom` is where the climb starts — the folder itself when
	 * sharing a folder, the PARENT folder when sharing a file.
	 * `selfFolderId` names the one folder whose grants are direct and
	 * must not be double-listed; a file passes none, because the walk
	 * begins above it and every grant it returns is genuinely inherited.
	 *
	 * Drives are excluded by the caller: a drive is the top of the chain
	 * and inherits from nothing.
	 *
	 * Failure is swallowed on purpose: the direct list answers "who did I
	 * share this with", and losing the inherited half should degrade the
	 * dialog, not empty it. The likely failure is a 403 —
	 * `include_grants=true` requires Share, and a caller can reach this
	 * dialog with less.
	 */
	async function loadInherited(
		walkFrom: string,
		selfFolderId?: string
	): Promise<{ people: Member[]; links: InheritedLink[] }> {
		try {
			const chain = await getFolderAncestorsWithGrants(walkFrom);
			const grants = chain.effective_grants ?? [];
			const nameById = new Map(chain.ancestors.map((a) => [a.id, a.name]));
			const driveName = chain.access_source.drive?.name ?? t('share.the_drive', 'the drive');

			// The owning drive, captured here because this walk already
			// resolves it — asking again would be a second round trip for
			// something we are holding. Its policies gate the link form
			// below; see `linkPolicies`.
			driveId = chain.access_source.drive?.id ?? chain.ancestors[0]?.drive_id ?? null;

			// Drop grants on the item itself — those are the direct ones,
			// already loaded, and listing them twice would double every row.
			const inheritedGrants = grants.filter(
				(g) => !(selfFolderId && g.resource.type === 'folder' && g.resource.id === selfFolderId)
			);

			// Public links on an ancestor. They belong on the Link tab, not
			// among people — a link is not a subject you can name, notify
			// or assign a role to.
			// They arrive pre-separated, carrying their own name and
			// password flag — a bare token grant would leave nothing to
			// show but an opaque id.
			const links: InheritedLink[] = (chain.effective_links ?? [])
				.filter(
					(l) => !(selfFolderId && l.resource.type === 'folder' && l.resource.id === selfFolderId)
				)
				.map((l) => ({
					grantId: l.grant_id,
					name: l.name || t('share.sharedLink', 'Shared link'),
					hasPassword: l.has_password,
					label: l.resource.type === 'drive' ? driveName : (nameById.get(l.resource.id) ?? ''),
					folderId: l.resource.type === 'drive' ? null : l.resource.id,
					driveId: l.resource.type === 'drive' ? l.resource.id : null,
					expiry: isoToDate(l.expires_at)
				}));

			const people = inheritedGrants.flatMap((g) => {
				if (g.subject.type === 'token') return [];
				const isDrive = g.resource.type === 'drive';
				return [
					{
						subject: g.subject,
						recipient: resolveRecipient(g.subject.type as 'user' | 'group', g.subject.id),
						role: displayRole(g.role),
						grantIds: [g.id],
						expiry: isoToDate(g.expires_at),
						inherited: {
							label: isDrive ? driveName : (nameById.get(g.resource.id) ?? ''),
							folderId: isDrive ? null : g.resource.id,
							driveId: isDrive ? g.resource.id : null
						}
					}
				];
			});

			return { people, links };
		} catch {
			return { people: [], links: [] };
		}
	}

	/**
	 * Pick the walk start for `item`'s kind.
	 *
	 * - **folder** — climb from itself, excluding its own grants (direct).
	 * - **file** — climb from its parent folder. That id is not on
	 *   `Target` (`{id, name, kind}`), so it costs one `getFile` first.
	 *   Worth it: a file's access is overwhelmingly inherited, so without
	 *   this the dialog answers "who can see this document?" with just
	 *   the handful of grants placed directly on it — which usually
	 *   means an empty list next to a file half the company can read.
	 * - **drive** — top of the chain, nothing above it to inherit from.
	 */
	async function loadInheritedFor(
		target: Target
	): Promise<{ people: Member[]; links: InheritedLink[] }> {
		if (target.kind === 'folder') return loadInherited(target.id, target.id);
		if (target.kind === 'file') {
			try {
				const file = await getFile(target.id);
				// No `selfFolderId`: the walk starts at the parent, so the
				// parent's own grants are inherited by the file, not direct.
				return await loadInherited(file.folder_id);
			} catch {
				return { people: [], links: [] };
			}
		}
		return { people: [], links: [] };
	}

	async function loadGrants() {
		if (!item) return;
		grantsLoading = true;
		try {
			await ensureResolvers();
			directoryAvailable = isDirectoryAvailable();
			const direct = groupGrants(await fetchGrantsForResource(item.kind, item.id));
			const inherited = await loadInheritedFor(item);
			// Direct first within each role group, so the rows a user can
			// actually act on sit above the read-only ones.
			members = [...direct, ...inherited.people];
			inheritedLinks = inherited.links;
		} catch (e) {
			errorToast(e);
		} finally {
			grantsLoading = false;
		}
	}

	function onQueryInput() {
		if (searchTimer) clearTimeout(searchTimer);
		searchTimer = setTimeout(async () => {
			const existing = new Set(members.map((m) => `${m.subject.type}:${m.subject.id}`));
			results = (await searchRecipients(query)).filter(
				(r) => !existing.has(`${r.type === 'email' ? 'user' : r.type}:${r.id}`)
			);
		}, 200);
	}

	function subjectInput(r: Recipient): GrantSubjectInput {
		if (r.type === 'email') return { type: 'email', email: r.id };
		return { type: r.type, id: r.id };
	}

	/** Choose a recipient; nothing is granted until [Add]. */
	function selectRecipient(r: Recipient) {
		selected = r;
		query = '';
		results = [];
	}

	async function addSelected() {
		if (!item || !selected || adding) return;
		adding = true;
		try {
			const res = await createGrant(
				subjectInput(selected),
				{ type: item.kind, id: item.id },
				newRole,
				expiryToIso(newExpiry)
			);
			// Only the selection is cleared: role and expiry are kept so
			// adding several people at the same access level is one click
			// each, which is the common case.
			selected = null;
			summarizeNotifications(res.notification.outcomes);
			onshared?.(item.id);
			onchange?.(item.id);
			await loadGrants();
		} catch (e) {
			errorToast(e);
		} finally {
			adding = false;
		}
	}

	async function changeRole(m: Member, role: ShareRole) {
		if (!item || role === m.role) return;
		try {
			await updateGrantRole(
				m.subject,
				{ type: item.kind, id: item.id },
				role,
				expiryToIso(m.expiry)
			);
			onchange?.(item.id);
			await loadGrants();
		} catch (e) {
			errorToast(e);
		}
	}

	async function changeMemberExpiry(m: Member, expiry: string | null) {
		if (!item) return;
		try {
			await updateGrantRole(
				m.subject,
				{ type: item.kind, id: item.id },
				m.role,
				expiryToIso(expiry)
			);
			onchange?.(item.id);
			await loadGrants();
		} catch (e) {
			errorToast(e);
		}
	}

	async function removeMember(m: Member) {
		try {
			for (const id of m.grantIds) await revokeGrant(id);
			if (item) onchange?.(item.id);
			await loadGrants();
		} catch (e) {
			errorToast(e);
		}
	}

	async function notifyMember(m: Member) {
		if (!m.notifyGrantId) return;
		try {
			const set = await notifyGrantRecipient(m.notifyGrantId);
			summarizeNotifications(set.outcomes);
		} catch (e) {
			errorToast(e);
		}
	}

	/** Aggregate notification outcomes into a single toast (mirrors OLD _surfaceNotifySummary). */
	function summarizeNotifications(outcomes: NotifyOutcome[]) {
		if (!outcomes || outcomes.length === 0) return;
		const sent = outcomes.filter((o) => o.kind === 'sent').length;
		const coalesced = outcomes.filter((o) => o.kind === 'coalesced').length;
		const rateLimited = outcomes.filter((o) => o.kind === 'rate_limited').length;
		const skipped = outcomes.filter((o) => o.kind === 'not_applicable').length;
		const lines: string[] = [];
		if (sent > 0) lines.push(t('share.notify.sent', { n: sent }, '{{n}} notified by email.'));
		if (coalesced > 0)
			lines.push(t('share.notify.coalesced', { n: coalesced }, '{{n}} already notified recently.'));
		if (rateLimited > 0)
			lines.push(
				t('share.notify.rateLimited', { n: rateLimited }, '{{n}} hit the rate limit — try later.')
			);
		if (skipped > 0)
			lines.push(
				t('share.notify.skipped', { n: skipped }, '{{n}} skipped (no email / opted out).')
			);
		if (lines.length === 0) return;
		const onlySent = coalesced === 0 && rateLimited === 0 && skipped === 0;
		ui.notify(lines.join(' '), onlySent ? 'success' : 'info');
	}

	// Members grouped by role, highest privilege first.
	const memberGroups = $derived(
		ROLE_ORDER.map((role) => ({
			role,
			members: members.filter((m) => m.role === role)
		})).filter((g) => g.members.length > 0)
	);

	// ── Public link ──────────────────────────────────────────────────────────
	let shares = $state<ShareItem[]>([]);
	let linkLoading = $state(false);
	let creating = $state(false);
	let newLinkName = $state('');
	let password = $state('');
	let expiresAt = $state<string | null>(null);

	// ── Drive policy, applied to the link form ───────────────────────────────
	//
	// The server enforces these at creation and REFUSES rather than clamps
	// — deliberately, because silently handing back a link that expires
	// sooner than asked is a dialog disagreeing with the person using it.
	// That choice only works if the limits are visible BEFORE submitting,
	// which is what this section is for. Without it the user meets the
	// policy as a 405 after committing.
	//
	// Resolved from the drives store rather than a dedicated endpoint:
	// `GET /api/drives` already returns EFFECTIVE policies (the kind's
	// default with this drive's overrides on top), which is exactly what
	// the server will enforce.
	let driveId = $state<string | null>(null);

	const linkPolicies = $derived.by(() => {
		if (!driveId) return null;
		const d = drivesStore.drives.find((x) => x.id === driveId);
		return d ? readAllPolicies(d.policies as Record<string, unknown>) : null;
	});

	/** Latest date a new link may expire, as YYYY-MM-DD, or null for no cap. */
	const maxExpiryDate = $derived.by(() => {
		const days = linkPolicies?.max_public_link_days;
		if (typeof days !== 'number' || days <= 0) return null;
		// Constructed from a timestamp rather than mutated with
		// `setDate` — the lint bans mutable Date instances, and UTC
		// arithmetic matches the server, which adds `Duration::days`.
		return new Date(Date.now() + days * 86_400_000).toISOString().slice(0, 10);
	});

	const passwordRequired = $derived(linkPolicies?.require_public_link_password === true);

	/**
	 * The drive's link rules, stated whether or not they are being broken.
	 *
	 * `linkBlockedReason` below only speaks once the user has already hit a
	 * limit, which left the constraint invisible until it bit: the Create
	 * button sat disabled with no indication that a policy existed at all,
	 * and someone who does not administer the drive has no way to guess one
	 * does. Saying the rule up front turns a dead control into an
	 * instruction.
	 *
	 * Null when the drive constrains nothing, so an unrestricted share keeps
	 * its quiet dialog.
	 */
	const linkPolicyHint = $derived.by(() => {
		if (!linkPolicies || linkPolicies.forbid_public_links) return null;
		const parts: string[] = [];
		const days = linkPolicies.max_public_link_days;
		if (typeof days === 'number' && days > 0) {
			parts.push(
				t('share.policy_hint_expiry', { n: days }, 'links must expire within {{n}} day(s)')
			);
		}
		if (passwordRequired) {
			parts.push(t('share.policy_hint_password', 'links need a password'));
		}
		if (parts.length === 0) return null;
		// One sentence naming the drive's rules, rather than a bullet list —
		// there are at most two, and a list would outweigh the form.
		return t(
			'share.policy_hint',
			{ rules: parts.join(t('share.policy_hint_join', ', and ')) },
			'This drive limits sharing: {{rules}}.'
		);
	});

	/**
	 * Why the form cannot be submitted, or null when it can.
	 *
	 * Mirrors the server's gates so the refusal never has to happen. A
	 * missing expiry counts as over the cap: a link that never expires is
	 * the laxest value there is, not an exemption — the same rule the
	 * backend applies.
	 */
	const linkBlockedReason = $derived.by(() => {
		if (!linkPolicies) return null;
		if (linkPolicies.forbid_public_links) {
			return t('share.policy_no_links', 'This drive does not allow public links.');
		}
		if (passwordRequired && !password.trim()) {
			return t('share.policy_password_required', 'This drive requires a password on public links.');
		}
		if (maxExpiryDate) {
			if (!expiresAt) {
				return t(
					'share.policy_expiry_required',
					{ date: maxExpiryDate },
					'This drive caps public links — set an expiry on or before {{date}}.'
				);
			}
			if (expiresAt > maxExpiryDate) {
				return t(
					'share.policy_expiry_too_far',
					{ date: maxExpiryDate },
					'This drive caps public links at {{date}}.'
				);
			}
		}
		return null;
	});

	// Tab counts. Declared here, after `shares` — both badges read state
	// that lives on either side of the People/Link split.
	//
	// Inherited entries count. The question a badge answers is "can
	// anyone reach this?", and access through an ancestor counts exactly
	// as much as access granted here; a tab reading (0) beside a folder
	// half the company can open would be the same lie the old "No public
	// links yet." told.
	const peopleCount = $derived(members.length);
	const linkCount = $derived(shares.length + inheritedLinks.length);

	async function loadShares() {
		// The share-link API only supports file/folder items; the Link tab
		// is hidden for drives (`allowLinks=false`) so this path is
		// unreachable, but narrow the type here so TypeScript doesn't
		// surface the widened `GrantResourceType` from `item.kind`.
		if (!item || item.kind === 'drive') return;
		linkLoading = true;
		try {
			shares = await listSharesForItem(item.id, item.kind);
		} catch (e) {
			errorToast(e);
		} finally {
			linkLoading = false;
		}
	}

	async function createLink() {
		if (!item || item.kind === 'drive') return;
		// Belt to the UI's braces: the button is disabled while a reason
		// stands, but a keyboard submit or a policy that changed under an
		// open dialog would otherwise reach the server and come back 405.
		if (linkBlockedReason) return;
		creating = true;
		try {
			await createShare({
				itemId: item.id,
				itemName: newLinkName.trim() || item.name,
				itemType: item.kind,
				password: password || null,
				expiresAt: expiresAt || null
			});
			newLinkName = '';
			password = '';
			expiresAt = null;
			onshared?.(item.id);
			onchange?.(item.id);
			await loadShares();
			ui.notify(t('share.created', 'Public link created'), 'success');
		} catch (e) {
			errorToast(e);
		} finally {
			creating = false;
		}
	}

	async function editLinkExpiry(share: ShareItem, expiry: string | null) {
		try {
			await updateShare(share.id, { expiresAt: expiry });
			if (item) onchange?.(item.id);
			await loadShares();
		} catch (e) {
			errorToast(e);
		}
	}

	async function editLinkPassword(share: ShareItem, pw: string | null) {
		try {
			await updateShare(share.id, { password: pw });
			if (item) onchange?.(item.id);
			await loadShares();
			ui.notify(
				pw
					? t('share.password_set', 'Password updated')
					: t('share.password_cleared', 'Password removed'),
				'success'
			);
		} catch (e) {
			errorToast(e);
		}
	}

	async function removeLink(share: ShareItem) {
		try {
			await deleteShare(share.id);
			if (item) onchange?.(item.id);
			shares = shares.filter((s) => s.id !== share.id);
		} catch (e) {
			errorToast(e);
		}
	}

	async function copy(url: string) {
		if (await copyShareLink(url)) ui.notify(t('share.copied', 'Link copied'), 'success');
		else ui.notify(t('share.copy_failed', 'Could not copy link'), 'error');
	}

	function shareExpiryIso(s: ShareItem): string | null {
		return s.expires_at ? new Date(s.expires_at * 1000).toISOString().slice(0, 10) : null;
	}

	$effect(() => {
		if (open && item) {
			void loadGrants();
			void loadShares();
			// Idempotent and usually already resolved (the sidebar picker
			// loads it at boot). Needed because the link form reads the
			// owning drive's effective policies out of it.
			void drivesStore.load();
		}
	});
</script>

<!-- ── Reusable expiry chip ───────────────────────────────────────────────

     Both branches drive a `<input type="date">` and open the native
     picker via `HTMLInputElement.showPicker()`. The previous invisible-
     overlay trick (an `opacity: 0` input covering a chip label) was
     unreliable — some browsers refuse to open the picker for a hidden
     input, which is why "Set expiry" appeared inert. `showPicker()`
     is the modern, explicit path and works from a button click.

     `min={todayIso()}` (from `../../api/endpoints/grants`) hard-caps
     the picker to today-or-later so a past date can't be selected.
     The `onchange` handler mirrors the same check as a belt-and-braces
     guard against the min attribute being ignored.  -->
{#snippet expiryChip(value: string | null, onchange: (v: string | null) => void)}
	{@const today = todayIso()}
	<span class="chip-edit">
		{#if value}
			<input
				class="chip-edit__date"
				type="date"
				value={value ?? ''}
				min={today}
				onchange={(e) => {
					const v = (e.currentTarget as HTMLInputElement).value;
					if (v && v < today) return;
					onchange(v || null);
				}}
				aria-label={t('share.expiry', 'Expiry')}
			/>
			<button
				class="chip-edit__clear"
				title={t('actions.clear', 'Clear')}
				onclick={() => onchange(null)}
				aria-label={t('actions.clear', 'Clear')}>×</button
			>
		{:else}
			<span class="chip-edit__ghost">
				<button
					type="button"
					class="chip chip--ghost"
					aria-label={t('share.set_expiry', 'Set expiry')}
					onclick={(e) => {
						const picker = (e.currentTarget as HTMLElement)
							.nextElementSibling as HTMLInputElement | null;
						picker?.showPicker?.();
						picker?.focus();
					}}
				>
					<Icon name="infinity" />
					<span>{t('share.noExpiry', 'No expiry')}</span>
				</button>
				<input
					class="chip-edit__date chip-edit__date--offscreen"
					type="date"
					min={today}
					aria-hidden="true"
					tabindex="-1"
					onchange={(e) => {
						const v = (e.currentTarget as HTMLInputElement).value;
						if (v && v < today) return;
						onchange(v || null);
					}}
				/>
			</span>
		{/if}
	</span>
{/snippet}

<Modal bind:open title={t('share.dialog_title', { name: item?.name ?? '' }, 'Share “{{name}}”')}>
	<div data-testid="share-dialog">
		<!-- People/Link tab switcher. Hidden entirely when `allowLinks=false`
		     (the drive-members surface) — with only one tab visible the
		     switcher would be visual noise. -->
		{#if allowLinks}
			<div class="tabs" role="tablist">
				<button
					role="tab"
					data-testid="share-dialog-people-tab"
					aria-selected={tab === 'people'}
					onclick={() => (tab = 'people')}
				>
					{t('share.people', 'People')}
					<!--
						Counts so both answers are visible without opening either
						tab — the common question is "is this shared at all?", and
						that used to cost a click into each. Inherited entries are
						included: they are access, and a tab reading (0) beside a
						folder half the company can reach would be the same lie the
						old "No public links yet." told.
					-->
					{#if peopleCount > 0}<span class="tab-badge">{peopleCount}</span>{/if}
				</button>
				<button
					role="tab"
					data-testid="share-dialog-link-tab"
					aria-selected={tab === 'link'}
					onclick={() => (tab = 'link')}
				>
					{t('share.public_link', 'Public link')}
					{#if linkCount > 0}<span class="tab-badge">{linkCount}</span>{/if}
				</button>
			</div>
		{/if}

		{#if tab === 'people'}
			{#if !directoryAvailable && !grantsLoading}
				<p class="status status--note">
					{t('share.directoryUnavailable', 'User directory unavailable')}
				</p>
			{:else}
				<div class="add-row">
					<div class="search">
						<!--
							Either the search box or the chosen recipient, never both:
							once someone is picked the search has done its job, and
							leaving the input there invited a second name to be typed
							over a selection that was about to be granted.
						-->
						{#if selected}
							<div class="sh-picked" data-testid="share-dialog-selected">
								<Icon
									name={selected.type === 'group'
										? 'user-group'
										: selected.type === 'email'
											? 'envelope'
											: 'user'}
								/>
								<span class="sh-picked__label">{selected.label}</span>
								<button
									class="btn-action"
									data-testid="share-dialog-clear-selection"
									title={t('share.clear_selection', 'Choose someone else')}
									onclick={() => (selected = null)}><Icon name="times" /></button
								>
							</div>
						{:else}
							<input
								data-testid="share-dialog-search-input"
								placeholder={t('share.add_people', 'Add people, groups, or email…')}
								bind:value={query}
								oninput={onQueryInput}
								autocomplete="off"
							/>
						{/if}
						{#if results.length > 0}
							<ul class="results">
								{#each results as r (r.type + r.id)}
									<li>
										<button
											class="result"
											data-testid={`share-dialog-result-${r.type}-${r.id}`}
											onclick={() => selectRecipient(r)}
										>
											<Icon
												name={r.type === 'group'
													? 'user-group'
													: r.type === 'email'
														? 'envelope'
														: 'user'}
											/>
											<span class="result__label">{r.label}</span>
											{#if r.type === 'email'}
												<span class="result__sub"
													>{t('share.inviteByEmail', 'Invite by email')}</span
												>
											{:else if r.sublabel}
												<span class="result__sub">{r.sublabel}</span>
											{/if}
										</button>
									</li>
								{/each}
							</ul>
						{/if}
					</div>
					<select
						class="role-select"
						data-testid="share-dialog-new-role-select"
						bind:value={newRole}
						aria-label={t('share.role_label', 'Role')}
					>
						{#each ROLES as r (r.v)}<option value={r.v}>{r.l}</option>{/each}
					</select>
					{@render expiryChip(newExpiry, (v) => (newExpiry = v))}
					<!--
						The commit. Role and expiry above are only meaningful because
						this exists: while picking a result granted access outright,
						they had to be set before the name was typed, so nobody did.

						Still immediate — the dialog gains no Save. Its own single
						button stays [Close], and this adds one recipient per press.
					-->
					<button
						class="btn btn-primary add-row__submit"
						data-testid="share-dialog-add-btn"
						disabled={!selected || adding}
						onclick={addSelected}
					>
						{adding ? t('share.adding', 'Adding…') : t('share.add', 'Add')}
					</button>
				</div>
			{/if}

			{#if grantsLoading}
				<div class="skeleton" aria-hidden="true">
					<div class="skeleton__line skeleton__line--short"></div>
					<div class="skeleton__line skeleton__line--medium"></div>
					<div class="skeleton__line"></div>
				</div>
			{:else if members.length === 0}
				<p class="status">{t('share.no_people', 'Not shared with anyone yet.')}</p>
			{:else}
				{#each memberGroups as group (group.role)}
					<div class="member-group">
						<div class="member-group__header">
							<Icon name={roleIcon(group.role)} />
							<span>{roleLabel(group.role)}</span>
							<span class="member-group__badge">{group.members.length}</span>
						</div>
						<ul class="members">
							<!--
								Keyed by subject AND source: the same person can hold a
								direct grant here and an inherited one from above, and
								those are two distinct rows. Keying on subject alone
								would collapse them and Svelte would reuse one row for
								both.
							-->
							{#each group.members as m (m.subject.type + m.subject.id + (m.inherited?.folderId ?? (m.inherited ? 'drive' : 'direct')))}
								<li
									class="member"
									class:member--inherited={m.inherited}
									class:member--expired={m.expiry && new Date(m.expiry) < new Date()}
								>
									{#if m.subject.type === 'user'}
										<UserVignette
											userId={m.subject.id}
											fallbackLabel={m.recipient.label}
											fallbackSublabel={m.recipient.sublabel}
										/>
									{:else}
										<!-- Sized to match `UserVignette`'s 32px avatar: a user row
										     and a group row are the same kind of thing in this
										     list, and a bare glyph beside a 32px circle made the
										     rows look misaligned rather than merely different. -->
										<span class="member__group-badge"><Icon name="user-group" /></span>
										<span class="member__label">
											{m.recipient.label}
											{#if m.recipient.sublabel}<span class="member__sub"
													>{m.recipient.sublabel}</span
												>{/if}
										</span>
									{/if}
									{#if m.inherited}
										<!--
											Inherited access is shown, never edited. The grant
											lives on another resource, so a role change or a
											revoke here would quietly alter a DIFFERENT folder's
											sharing — and would surprise whoever set it. The chip
											is the way through: it opens that folder, where the
											grant is direct and editable.

											Rendering the controls disabled instead was the
											alternative; a link that leads somewhere beats three
											dead controls that explain nothing.
										-->
										{#if m.inherited.folderId}
											{@const src = m.inherited}
											<!--
												Link AND re-target. The href moves the page behind
												the dialog (and keeps middle-click / open-in-new-tab
												working); `onretarget` re-points the dialog itself,
												which a same-route navigation would otherwise leave
												stranded on the folder the user came from.
											-->
											<a
												class="member__source"
												href={resolve(`/files/${src.folderId}`)}
												data-testid={`share-dialog-member-source-${m.subject.type}-${m.subject.id}`}
												title={t(
													'share.inherited_from_folder_title',
													{ name: src.label },
													'Inherited from folder "{{name}}" — open it to change this'
												)}
												onclick={() =>
													src.folderId &&
													onretarget?.({ id: src.folderId, name: src.label, kind: 'folder' })}
											>
												<Icon name="level-up-alt" />
												<span>{m.inherited.label}</span>
											</a>
										{:else if m.inherited.driveId}
											{@const did = m.inherited.driveId}
											<!--
												Drives go to their config page, not `/files`: drive
												membership is a different grant type — its own role
												ladder, no public links — and that page runs the
												drive-kind dialog. No `onretarget`, for the same
												reason: re-pointing THIS dialog at a drive would
												offer folder controls for a resource that does not
												take them.
											-->
											<a
												class="member__source"
												href={resolve(`/config/drive/${did}`)}
												data-testid={`share-dialog-member-source-${m.subject.type}-${m.subject.id}`}
												title={t(
													'share.inherited_from_drive_title',
													{ name: m.inherited.label },
													'Inherited from drive "{{name}}" — open its settings to change this'
												)}
											>
												<Icon name="hdd" />
												<span>{m.inherited.label}</span>
											</a>
										{/if}
									{:else}
										{@render expiryChip(m.expiry, (v) => changeMemberExpiry(m, v))}
										<select
											class="role-select"
											data-testid={`share-dialog-member-role-${m.subject.type}-${m.subject.id}`}
											value={m.role}
											onchange={(e) => changeRole(m, e.currentTarget.value as ShareRole)}
										>
											{#each ROLES as r (r.v)}<option value={r.v}>{r.l}</option>{/each}
										</select>
										<button
											class="btn-action"
											data-testid={`share-dialog-member-notify-${m.subject.type}-${m.subject.id}`}
											title={t('share.notifyByEmail', 'Notify by email')}
											onclick={() => notifyMember(m)}><Icon name="paper-plane" /></button
										>
										<button
											class="btn-action btn-action--delete"
											data-testid={`share-dialog-member-remove-${m.subject.type}-${m.subject.id}`}
											title={t('share.revoke', 'Remove')}
											onclick={() => removeMember(m)}><Icon name="user-xmark" /></button
										>
									{/if}
								</li>
							{/each}
						</ul>
					</div>
				{/each}
			{/if}
		{:else}
			<section class="sh-create">
				<div class="sh-fields">
					<label>
						<span>{t('share.link_name', 'Link name (optional)')}</span>
						<input
							type="text"
							data-testid="share-dialog-link-name-input"
							bind:value={newLinkName}
							autocomplete="off"
						/>
					</label>
					<label>
						<span>
							{passwordRequired
								? t('share.password_required', 'Password (required)')
								: t('share.password_optional', 'Password (optional)')}
						</span>
						<input
							type="text"
							data-testid="share-dialog-link-password-input"
							bind:value={password}
							required={passwordRequired}
							autocomplete="off"
						/>
					</label>
					<label>
						<span>
							{maxExpiryDate
								? t('share.expires_required', 'Expires (required)')
								: t('share.expires_optional', 'Expires (optional)')}
						</span>
						<!-- `max` caps the native picker, so the limit is visible
						     in the calendar itself rather than only after a
						     refusal. The submit guard still checks, because
						     `max` is advisory in some browsers. -->
						<input
							type="date"
							data-testid="share-dialog-link-expires-input"
							value={expiresAt ?? ''}
							min={todayIso()}
							max={maxExpiryDate ?? undefined}
							onchange={(e) => {
								const v = e.currentTarget.value;
								if (v && v < todayIso()) return;
								expiresAt = v || null;
							}}
						/>
					</label>
				</div>
				<!--
					One slot, two modes. While the form is submittable this states
					the drive's rules; once a limit is hit it states the reason the
					button is off. The blocking form is styled as a constraint
					rather than as a hint, because muted grey beside a disabled
					button read as incidental text and left the button looking
					broken instead of governed.
				-->
				{#if linkBlockedReason}
					<p
						class="sh-policy sh-policy--blocked"
						id="share-link-policy"
						data-testid="share-dialog-policy-block"
					>
						<Icon name="shield-alt" />
						{linkBlockedReason}
					</p>
				{:else if linkPolicyHint}
					<p class="sh-policy" id="share-link-policy" data-testid="share-dialog-policy-hint">
						<Icon name="shield-alt" />
						{linkPolicyHint}
					</p>
				{/if}
				<button
					class="btn btn-primary sh-create__submit"
					data-testid="share-dialog-create-btn"
					disabled={creating || linkBlockedReason !== null}
					onclick={createLink}
					aria-describedby={linkBlockedReason || linkPolicyHint ? 'share-link-policy' : undefined}
					title={linkBlockedReason ?? undefined}
				>
					{t('share.create_link', 'Create link')}
				</button>
			</section>

			{#if linkLoading}
				<div class="skeleton" aria-hidden="true">
					<div class="skeleton__line skeleton__line--medium"></div>
					<div class="skeleton__line"></div>
				</div>
			{:else if shares.length === 0}
				<!--
					Deliberately narrower than it used to read. "No public
					links yet." was a statement about EXPOSURE, and it was
					wrong whenever an ancestor carried a link: the item was
					publicly reachable while the dialog said it wasn't. It
					now only claims nothing was published *here*, and the
					inherited section below supplies the rest.
				-->
				<p class="status">
					{inheritedLinks.length > 0
						? t('share.none_direct', 'No public link on this item itself.')
						: t('share.none', 'No public links yet.')}
				</p>
			{:else}
				<ul class="links">
					{#each shares as s (s.id)}
						<li class="link-row">
							<span class="link-row__title">
								<Icon name={s.has_password ? 'lock' : 'link'} />
								<span class="link-row__name"
									>{s.item_name || t('share.sharedLink', 'Shared link')}</span
								>
							</span>
							{@render expiryChip(shareExpiryIso(s), (v) => editLinkExpiry(s, v))}
							<button
								class="btn-action"
								class:btn-action--on={s.has_password}
								data-testid={`share-dialog-link-password-btn-${s.id}`}
								title={s.has_password
									? t('share.changePassword', 'Change password')
									: t('share.addPassword', 'Add password')}
								onclick={() => {
									const pw = window.prompt(
										s.has_password
											? t('share.passwordPrompt_clear', 'New password (blank to remove):')
											: t('share.passwordPrompt', 'Set a password:')
									);
									if (pw !== null) editLinkPassword(s, pw || null);
								}}><Icon name={s.has_password ? 'lock' : 'lock-open'} /></button
							>
							<button
								class="btn-action"
								data-testid={`share-dialog-link-copy-btn-${s.id}`}
								title={t('share.copy', 'Copy')}
								onclick={() => copy(s.url)}
							>
								<Icon name="copy" />
							</button>
							<button
								class="btn-action btn-action--delete"
								data-testid={`share-dialog-link-delete-btn-${s.id}`}
								title={t('common.delete', 'Delete')}
								onclick={() => removeLink(s)}><Icon name="trash" /></button
							>
						</li>
					{/each}
				</ul>
			{/if}

			<!--
				Public links on an ancestor. These reach this item too, so
				omitting them would let the tab imply the item is private
				when it is on the open internet.

				Read-only, like inherited people: the link belongs to
				another folder, and deleting it from here would revoke
				access to everything else under that folder. The chip goes
				to the source instead. No URL shown — the walk returns the
				token grant, not the share row, so there is no link to copy
				without a second lookup against a folder the caller may not
				be entitled to enumerate.
			-->
			{#if inheritedLinks.length > 0}
				<section class="inherited-links">
					<h3 class="inherited-links__title">
						<Icon name="link" />
						{t('share.inherited_links_title', 'Also reachable through a public link')}
					</h3>
					<ul class="links">
						{#each inheritedLinks as l (l.grantId)}
							<li class="link-row link-row--inherited">
								<!--
									Same shape as a direct link row above — lock glyph when
									password-protected, then the link's own name — which is
									also the shape of a person row: identity on the left,
									source chip on the right. These rows differ from direct
									ones in what you can DO with them, not in how they read.
								-->
								<span class="link-row__title">
									<Icon name={l.hasPassword ? 'lock' : 'link'} />
									<span class="link-row__name">{l.name}</span>
								</span>
								{#if l.expiry}
									<span class="link-row__expiry">{l.expiry}</span>
								{/if}
								{#if l.folderId}
									{@const fid = l.folderId}
									<a
										class="member__source"
										href={resolve(`/files/${fid}`)}
										data-testid={`share-dialog-inherited-link-source-${l.grantId}`}
										title={t(
											'share.inherited_link_manage',
											{ name: l.label },
											'Open “{{name}}” to manage or remove this link'
										)}
										onclick={() => onretarget?.({ id: fid, name: l.label, kind: 'folder' })}
									>
										<Icon name="level-up-alt" />
										<span>{l.label}</span>
									</a>
								{:else if l.driveId}
									{@const did = l.driveId}
									<a
										class="member__source"
										href={resolve(`/config/drive/${did}`)}
										data-testid={`share-dialog-inherited-link-source-${l.grantId}`}
										title={t(
											'share.inherited_link_on_drive_title',
											{ name: l.label },
											'This link lives on drive “{{name}}” — open its settings'
										)}
									>
										<Icon name="hdd" />
										<span>{l.label}</span>
									</a>
								{/if}
							</li>
						{/each}
					</ul>
				</section>
			{/if}
		{/if}
	</div>

	{#snippet footer()}
		<button
			class="btn btn-secondary"
			data-testid="share-dialog-close-btn"
			onclick={() => (open = false)}
		>
			{t('common.close', 'Close')}
		</button>
	{/snippet}
</Modal>

<style>
	.tabs {
		display: flex;
		gap: var(--space-1);
		border-bottom: 1px solid var(--color-border);
		margin-bottom: var(--space-4);
	}

	.tabs button {
		padding: var(--space-2) var(--space-3);
		border: none;
		background: none;
		color: var(--color-text-muted);
		cursor: pointer;
		border-bottom: 2px solid transparent;
	}

	.tabs button[aria-selected='true'] {
		color: var(--color-text);
		border-bottom-color: var(--color-accent);
	}

	/*
	 * Same treatment as the link tab's form: separated from the list below it
	 * and given the same room, because adding someone and reviewing who
	 * already has access are two different jobs in one panel.
	 */
	.add-row {
		display: flex;
		gap: var(--space-2);
		padding-bottom: var(--space-4);
		border-bottom: 1px solid var(--color-border);
		margin-bottom: var(--space-4);
		align-items: center;
		flex-wrap: wrap;
	}

	/* Far right, on whichever line it ends up on. `.search` is `flex: 1`, so
	   on a wide dialog this sits right already — but the row wraps, and
	   without the auto margin the button starts from the left of the second
	   line the moment the dialog is narrow. */
	.add-row__submit {
		margin-left: auto;
	}

	.search {
		position: relative;
		flex: 1;
		min-width: 12rem;
	}

	.search input,
	.role-select,
	.sh-fields input {
		padding: var(--space-2) var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
	}

	.search input {
		width: 100%;
	}

	/* The chosen recipient, occupying the search box's place. Bordered like
	   an input so the row keeps its shape when the field is swapped out. */
	.sh-picked {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		min-width: 0;
		padding: var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-surface);
	}

	.sh-picked__label {
		flex: 1 1 auto;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.results {
		position: absolute;
		left: 0;
		right: 0;
		top: 100%;
		z-index: 10;
		list-style: none;
		margin: var(--space-1) 0 0;
		padding: var(--space-1);
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		box-shadow: var(--shadow-lg);
		max-height: 14rem;
		overflow: auto;
	}

	.result {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		width: 100%;
		padding: var(--space-2);
		border: none;
		background: none;
		color: var(--color-text);
		cursor: pointer;
		border-radius: var(--radius-sm);
		text-align: left;
	}

	.result:hover {
		background: var(--color-bg-hover);
	}

	.result__label {
		flex: 1;
	}

	.result__sub {
		color: var(--color-text-muted);
		font-size: var(--text-sm);
	}

	.member-group {
		margin-bottom: var(--space-3);
	}

	/* Air between groups, not just under each heading. "Who can manage" versus
	   "who can only view" is the question a reader scans this list for, so the
	   groups have to read as separate blocks rather than one run of rows with
	   labels sprinkled through it. */
	.member-group + .member-group {
		margin-top: var(--space-5);
	}

	/*
	 * Body text colour, not muted grey.
	 *
	 * At `--color-text-muted` these headings were quieter than the member
	 * names beneath them, so they read as a caption belonging to the first row
	 * rather than as the label for the group — the access level, which is the
	 * most important thing on the row, was the least visible thing in the
	 * list. Slight letter-spacing keeps it reading as a section label now
	 * that it carries full contrast, rather than as another name.
	 */
	.member-group__header {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		font-size: var(--text-sm);
		font-weight: var(--weight-semibold, 600);
		letter-spacing: 0.02em;
		color: var(--color-text);
		margin-bottom: var(--space-3);
	}

	.member-group__badge {
		min-width: 1.25rem;
		text-align: center;
		padding: 0 var(--space-1);
		border-radius: var(--radius-pill, 999px);
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
		font-size: var(--text-xs, 0.75rem);
	}

	.members,
	.links {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}

	.member {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	.member--expired {
		opacity: 0.6;
	}

	/* Inherited access. Muted rather than hidden: these people CAN reach
	   the folder, so leaving them out would answer "who has access?"
	   wrongly — but they are not editable here, and the weight difference
	   is what says so before anyone clicks. */
	.member--inherited {
		opacity: 0.75;
	}

	/* Where the inherited grant actually lives. A link when it is a
	   folder (go there and it becomes editable); flat text for a drive,
	   which has no browsable URL of its own — its grants are managed in
	   the drive settings. */
	.member__source {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		max-width: 14ch;
		padding: var(--space-0-5) var(--space-1);
		border-radius: var(--radius-sm);
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
		font-size: var(--text-xs);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		text-decoration: none;
	}

	a.member__source:hover {
		color: var(--color-accent);
		background: var(--color-accent-bg);
	}

	/* Inherited public links. Separated from the direct list by a rule
	   rather than a tab of their own: they answer the same question
	   ("is this reachable by link?") and splitting them would let
	   someone read only the first half. */
	/* Count pill in a tab label. Sized off the tab's own font so it
	   tracks the label rather than floating at a fixed size. */
	.tab-badge {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		min-width: 1.5em;
		margin-left: var(--space-1);
		padding: 0 0.4em;
		border-radius: 999px;
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
		font-size: 0.85em;
		font-variant-numeric: tabular-nums;
	}

	.tabs button[aria-selected='true'] .tab-badge {
		background: var(--color-accent-bg);
		color: var(--color-accent);
	}

	.inherited-links {
		margin-top: var(--space-4);
		padding-top: var(--space-3);
		border-top: 1px solid var(--color-border);
	}

	.inherited-links__title {
		display: flex;
		align-items: center;
		gap: var(--space-1);
		margin: 0 0 var(--space-2);
		font-size: var(--text-sm);
		font-weight: var(--weight-medium);
		color: var(--color-text-muted);
	}

	.link-row--inherited {
		opacity: 0.75;
	}

	.link-row__expiry {
		font-size: var(--text-xs);
		color: var(--color-text-muted);
	}

	/*
	 * Group counterpart to `UserVignette`'s avatar, at the same 32px circle.
	 *
	 * A group row and a user row are the same kind of entry in this list — a
	 * subject that has access — so their leading element has to occupy the
	 * same box, or the labels beside them do not line up and the list looks
	 * ragged rather than merely mixed. Kept muted: it is a category icon, not
	 * an identity, and it should not compete with the avatars for attention.
	 */
	.member__group-badge {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		flex-shrink: 0;
		width: 32px;
		height: 32px;
		border-radius: 50%;
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
	}

	.member__label {
		flex: 1;
		display: flex;
		flex-direction: column;
		overflow: hidden;
	}

	.member__sub {
		color: var(--color-text-muted);
		font-size: var(--text-sm);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	/* Why the Create button is disabled. Stated inline rather than as a
	   toast, because it is a standing condition of this drive, not an
	   event — and the user needs it while deciding what to type. */
	/*
	 * Divides creating a link from the links that already exist — two
	 * different jobs sharing one panel, and without a rule the Create button
	 * read as belonging to the first row of the list beneath it.
	 *
	 * On the form rather than on the list because three different things can
	 * follow it — the loading skeleton, the "no links yet" note, or the list
	 * itself — and the separation should hold for all three rather than be
	 * repeated on each.
	 */
	.sh-create {
		padding-bottom: var(--space-4);
		border-bottom: 1px solid var(--color-border);
		margin-bottom: var(--space-4);
	}

	/*
	 * The commit action for this form, bottom-right where a reader expects it.
	 *
	 * `width: fit-content` is the part that matters: `.btn` is
	 * `display: flex`, so as a block-level flex container it stretched to the
	 * full width of the section — which is why it read as a left-aligned
	 * banner rather than a button. Shrinking it to its content is what lets
	 * `margin-left: auto` push it right at all.
	 *
	 * Set apart from the fields above so it does not look like another one.
	 */
	.sh-create__submit {
		width: fit-content;
		margin-top: var(--space-4);
		margin-left: auto;
	}

	/* Informational mode: the drive's rules, stated before they are broken.
	   Quiet on purpose — nothing is wrong yet. */
	.sh-policy {
		display: flex;
		align-items: center;
		gap: 0.4rem;
		margin: 0.5rem 0 0;
		color: var(--color-text-muted);
		font-size: 0.875rem;
	}

	/* Blocking mode: this is the reason the Create button is disabled, so it
	   has to carry more weight than helper text. Muted grey here made a
	   governed button look like a broken one. Amber rather than red: the user
	   has not done anything wrong, there is a limit to work within. */
	.sh-policy--blocked {
		align-items: flex-start;
		padding: var(--space-2);
		border: 1px solid var(--color-warning-text);
		border-radius: var(--radius-md);
		background: var(--color-warning-bg);
		color: var(--color-warning-text);
		font-weight: 500;
	}

	.sh-fields {
		display: flex;
		gap: var(--space-3);
		margin-bottom: var(--space-3);
		flex-wrap: wrap;
	}

	.sh-fields label {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		flex: 1;
		min-width: 8rem;
		font-size: var(--text-sm);
	}

	.link-row {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	.link-row__title {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		flex: 1;
		overflow: hidden;
	}

	.link-row__name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.status {
		color: var(--color-text-muted);
		padding: var(--space-3) 0;
	}

	.status--note {
		font-style: italic;
	}

	.btn-action--delete:hover {
		color: var(--color-danger-text);
	}

	.btn-action--on {
		color: var(--color-accent);
	}

	/* ── Expiry chip ─────────────────────────────────────────────────────── */
	.chip-edit {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
	}

	.chip {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		padding: var(--space-1) var(--space-2);
		border-radius: var(--radius-pill, 999px);
		border: 1px solid var(--color-border);
		font-size: var(--text-sm);
		color: var(--color-text);
		cursor: pointer;
		position: relative;
	}

	.chip--ghost {
		border-style: dashed;
		/* WCAG-friendly foreground on both light and dark surfaces —
		   `--color-text-muted` was under the minimum AA contrast ratio,
		   making "No expiry" hard to read. Use the subtle-but-not-muted
		   text token instead, and give the ghost chip a low-tint
		   background so it visually separates from the modal body. */
		color: var(--color-text-subtle);
		background: var(--color-bg-input);
	}

	.chip--ghost:hover,
	.chip--ghost:focus-visible {
		color: var(--color-text);
		background: var(--color-border-subtle);
	}

	.chip-edit__date {
		padding: var(--space-1) var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
		font-size: var(--text-sm);
	}

	/* Positions the hidden `<input type="date">` off-screen (no `display:
	   none` — `showPicker()` refuses to open on a display:none input in
	   several browsers). The button next to it invokes `showPicker()`
	   programmatically. */
	.chip-edit__ghost {
		position: relative;
		display: inline-flex;
		align-items: center;
	}

	.chip-edit__date--offscreen {
		position: absolute;
		width: 1px;
		height: 1px;
		left: 0;
		bottom: 0;
		opacity: 0;
		pointer-events: none;
	}

	.chip-edit__clear {
		border: none;
		background: none;
		color: var(--color-text-muted);
		cursor: pointer;
		font-size: var(--text-md, 1rem);
		line-height: 1;
	}

	/* ── Loading skeleton ────────────────────────────────────────────────── */
	.skeleton {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		padding: var(--space-3) 0;
	}

	.skeleton__line {
		height: 1rem;
		border-radius: var(--radius-sm);
		background: linear-gradient(
			90deg,
			var(--color-bg-muted) 25%,
			var(--color-bg-hover) 37%,
			var(--color-bg-muted) 63%
		);
		background-size: 400% 100%;
		animation: shimmer 1.4s ease infinite;
	}

	.skeleton__line--short {
		width: 40%;
	}

	.skeleton__line--medium {
		width: 65%;
	}

	@keyframes shimmer {
		0% {
			background-position: 100% 0;
		}

		100% {
			background-position: 0 0;
		}
	}
</style>
