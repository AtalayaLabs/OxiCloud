<script lang="ts">
	import { errorToast } from '$lib/utils/errors';
	import { useAsyncAction } from '$lib/composables/useAsyncAction.svelte';
	import {
		copyShareLink,
		createShare,
		deleteShare,
		listSharesForItem,
		updateShare
	} from '$lib/api/endpoints/shares';
	import {
		createGrant,
		fetchGrantsForResource,
		notifyGrantRecipient,
		revokeGrant,
		updateGrantRole,
		type Grant,
		type GrantSubject,
		type GrantSubjectInput,
		type NotifyOutcome,
		type ShareRole
	} from '$lib/api/endpoints/grants';
	import { displayRole } from '$lib/utils/grantFormat';
	import { emitRowFlags } from '$lib/utils/listActions';
	import { expiryToIso, todayIso } from '$lib/utils/dateInput';
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
	import type { ItemType, ShareItem } from '$lib/api/types';
	import type { GrantTargetType } from '$lib/api/endpoints/grants';
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
		kind: GrantTargetType;
	}

	interface Props {
		open: boolean;
		item: Target | null;
		/**
		 * Fired with the item id on **any** membership mutation — create,
		 * role change, expiry change, removal, or public-link creation.
		 *
		 * For the row's shared CHIP there is nothing to wire: every mutation
		 * broadcasts the resource's new shared state through
		 * `listActions.emitRowFlags`, and each listing patches its own row
		 * (see `announceSharedState`). This prop is for callers that need to
		 * reload something else — the drive-config view re-reads its member
		 * table. A former `onshared` prop covered creation only and was
		 * wired by one page, which is why the chip went stale everywhere
		 * else.
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

	let { open = $bindable(false), item, onchange, allowLinks = true, onretarget }: Props = $props();

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
	/**
	 * `ROLES` reversed: view → edit → manage.
	 *
	 * The member list sorts most-privileged first, which is the right order
	 * for reading who can do what. A picker is the opposite job — the safest
	 * option should be the one you land on first — so the add form walks up
	 * instead of down. Derived from `ROLES` rather than written out again, so
	 * a new role cannot appear in one and not the other.
	 */
	const roleChoices = [...ROLES].reverse();
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
	 * Recipients chosen from the search results, not yet granted.
	 *
	 * Picking a result used to create the grant outright, which meant the
	 * role and expiry beside the search box only had any effect if the user
	 * had set them BEFORE typing a name — so in practice everyone was added
	 * as the default role and corrected afterwards. Holding the choices here
	 * lets the role be decided first, and makes [Add] the moment of commit.
	 *
	 * A LIST, because sharing with a handful of people at one access level is
	 * the normal case: picking a name used to swap the search box out for the
	 * chosen chip, so the second name could not be typed until the first had
	 * been granted — one round trip, one role press and one [Add] per person.
	 * Now the field stays, the picks queue up as chips, and [Add] grants them
	 * all at the role above.
	 */
	let picks = $state<Recipient[]>([]);
	/**
	 * The search field itself, so a pick or a grant can hand focus back to
	 * it. Adding several people is a typing job; losing the caret after each
	 * one turns it into a mouse job.
	 */
	let searchInput = $state<HTMLInputElement | null>(null);
	/**
	 * Stable identity for a recipient, in the SUBJECT vocabulary the member
	 * list uses — an `email` result becomes a `user` subject once granted, so
	 * it has to key as one here. Keying it as `email:` instead meant an
	 * invitee already queued, or already a member, was still offered by the
	 * search: two keys for one person, in one filter.
	 */
	function recipientKey(r: Recipient): string {
		return `${r.type === 'email' ? 'user' : r.type}:${r.id}`;
	}
	// Add-grant inline-error UX via the shared composable — a failed
	// `createGrant` (duplicate grant, policy-forbidden, authz denial)
	// renders inline below the add row instead of as a corner toast.
	// Per-row grant mutations (`changeRole`, `removeMember`, …) stay on
	// toast — attaching per-row error state is heavier than the UX win
	// warrants for inline-row controls. See
	// `frontend/AGENTS.md § Dialog error UX`.
	const addGrantAction = useAsyncAction();
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
			// Only DIRECT members are filtered out. Hiding everyone already
			// listed looked right and was wrong: someone whose access comes
			// from a parent folder appears in this dialog as a read-only
			// inherited row, so they were unsearchable — there was no way to
			// give them a grant HERE, which is exactly what you want when a
			// file moves out from under that parent, or when they need a
			// different role on this one resource. An inherited row is a
			// statement about an ancestor; it says nothing about this
			// resource's own grants.
			// Direct members AND anyone already queued: `selectRecipient`
			// dedupes, but offering a name that is sitting in a chip right
			// above the field invites a second press that looks like it did
			// nothing. Built in one expression rather than mutated — a plain
			// `Set` that is written to trips `svelte/prefer-svelte-reactivity`,
			// and this one is ephemeral, not state.
			const direct = new Set([
				...members.filter((m) => !m.inherited).map((m) => `${m.subject.type}:${m.subject.id}`),
				...picks.map(recipientKey)
			]);
			// Where the inherited access comes from, keyed the same way, so a
			// result can say so instead of looking like a duplicate.
			const inheritedFrom = new Map(
				members
					.filter((m) => m.inherited)
					.map((m) => [`${m.subject.type}:${m.subject.id}`, m.inherited!.label])
			);
			results = (await searchRecipients(query))
				.filter((r) => !direct.has(recipientKey(r)))
				.map((r) => {
					const source = inheritedFrom.get(recipientKey(r));
					if (!source) return r;
					// Appended, not replacing: the existing sublabel is usually
					// the email, which is how you tell two people apart.
					const note = t('share.already_via', { source }, 'already has access via {{source}}');
					return { ...r, sublabel: r.sublabel ? `${r.sublabel} · ${note}` : note };
				});
		}, 200);
	}

	function subjectInput(r: Recipient): GrantSubjectInput {
		if (r.type === 'email') return { type: 'email', email: r.id };
		return { type: r.type, id: r.id };
	}

	/** Queue a recipient; nothing is granted until [Add]. */
	function selectRecipient(r: Recipient) {
		if (!picks.some((p) => recipientKey(p) === recipientKey(r))) picks.push(r);
		// The query goes, the field stays: the next name is typed straight
		// into it. Results are dropped so the list does not linger over a
		// name already queued.
		query = '';
		results = [];
		// Picking happens by clicking a result, which takes focus — and that
		// result is then removed, leaving focus nowhere. Hand it back to the
		// field so a second name is just typing, no mouse trip.
		searchInput?.focus();
	}

	function unpick(r: Recipient) {
		picks = picks.filter((p) => recipientKey(p) !== recipientKey(r));
	}

	/**
	 * Tell every mounted listing whether this resource is still shared, so the
	 * row's chip tracks what the dialog just did.
	 *
	 * The bug this closes: sharing or unsharing left the chip as it was until
	 * a page refresh. `onshared` only ever covered CREATION and only `/files`
	 * wired it, so `/favorites`, `/recent`, `/search`, `/shared-with-me` and
	 * `/changes` painted a stale chip in both directions.
	 *
	 * The value mirrors the server's predicate exactly — `is_shared` is
	 * `EXISTS (SELECT 1 FROM storage.role_grants WHERE resource_id = <this>)`,
	 * so:
	 *   - INHERITED members don't count: their grant is on an ancestor, and
	 *     the server computes the flag per row, not per access path. Painting
	 *     a chip for them would contradict the next reload.
	 *   - Direct link shares DO count: a public link carries its own
	 *     `subject_type = 'token'` grant row on this resource.
	 *
	 * Called after each mutation RELOADS its list, never from the open
	 * effect's first pass — announcing from a half-loaded dialog would flash
	 * the chip off (members empty, shares not fetched yet) and back on.
	 */
	function announceSharedState() {
		if (!item) return;
		emitRowFlags({
			id: item.id,
			is_shared: members.some((m) => !m.inherited) || shares.length > 0
		});
	}

	async function addSelected() {
		if (!item || picks.length === 0 || addGrantAction.busy) return;
		await addGrantAction.run(async () => {
			// Sequential, not `Promise.all`: these are grant writes on one
			// resource and the server rejects some of them for policy reasons
			// (`drive_policies`), which is far easier to attribute to a person
			// when the calls are ordered. A handful of recipients is the
			// realistic size here, so the round trips are not worth racing.
			const outcomes: NotifyOutcome[] = [];
			const failed: Recipient[] = [];
			let firstError: unknown = null;
			for (const r of picks) {
				try {
					const res = await createGrant(
						subjectInput(r),
						{ type: item!.kind, id: item!.id },
						newRole,
						expiryToIso(newExpiry)
					);
					outcomes.push(...res.notification.outcomes);
				} catch (e) {
					failed.push(r);
					firstError ??= e;
				}
			}
			// The granted ones leave the queue and the refused ones stay, so a
			// retry is one press and nobody is granted twice. Role and expiry
			// are kept either way — adding several people at one access level
			// is the common case, and re-choosing the role each time was the
			// friction that made this a list in the first place.
			picks = failed;
			summarizeNotifications(outcomes);
			onchange?.(item!.id);
			await loadGrants();
			announceSharedState();
			// Same reason as after a pick: [Add] took the focus, and the next
			// thing anyone does in this dialog is usually type another name.
			searchInput?.focus();
			// A partial success must not look like a success: rethrowing the
			// first refusal puts the SERVER's reason inline (duplicate grant,
			// policy, authz) under a queue that still shows exactly who did
			// not make it.
			if (firstError !== null) throw firstError;
		});
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
			// The direction that had no signal at all before: removing the
			// last member must clear the row's chip, and removing one of
			// several must leave it alone. `announceSharedState` reads the
			// freshly reloaded list, so it answers both without counting here.
			announceSharedState();
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
	// Create-link inline-error UX via the shared composable. See the
	// `addGrantAction` comment above for the scope rationale.
	const createLinkAction = useAsyncAction();
	let newLinkName = $state('');
	let password = $state('');
	let expiresAt = $state<string | null>(null);
	let maxDownloads = $state<number | null>(null);

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

	const isLinkable = (kind: GrantTargetType): kind is ItemType =>
		kind === 'file' || kind === 'folder';

	async function loadShares() {
		// The share-link API only supports file/folder items; drives and
		// calendars have no links and no Link tab (`allowLinks=false`).
		if (!item || !isLinkable(item.kind)) return;
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
		if (!item || !isLinkable(item.kind)) return;
		// Belt to the UI's braces: the button is disabled while a reason
		// stands, but a keyboard submit or a policy that changed under an
		// open dialog would otherwise reach the server and come back 405.
		if (linkBlockedReason) return;
		// Capture the narrowed `item` + its narrowed kind into locals so
		// TypeScript's narrowing (`isLinkable`) survives the
		// arrow-function closure handed to `run()`. The non-null `item!`
		// alone would re-widen `kind` back to `GrantTargetType` and
		// mis-type the `createShare` argument as including 'drive'.
		const target = item;
		const targetKind: ItemType = item.kind;
		const ok = await createLinkAction.run(async () => {
			await createShare({
				itemId: target.id,
				itemName: newLinkName.trim() || target.name,
				itemType: targetKind,
				password: password || null,
				expiresAt: expiresAt || null,
				maxDownloads
			});
			newLinkName = '';
			password = '';
			expiresAt = null;
			maxDownloads = null;
			onchange?.(target.id);
			await loadShares();
			announceSharedState();
		});
		if (ok) {
			// Success toast — kept for the "something happened" breadcrumb;
			// only the failure path moved to inline.
			ui.notify(t('share.created', 'Public link created'), 'success');
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
			announceSharedState();
		} catch (e) {
			errorToast(e);
		}
	}

	async function copy(url: string) {
		if (await copyShareLink(url)) ui.notify(t('share.copied', 'Link copied'), 'success');
		else ui.notify(t('share.copy_failed', 'Could not copy link'), 'error');
	}

	function shareExpiryIso(s: ShareItem): string | null {
		// `s.expires_at` is already an RFC 3339 UTC string post-pivot; render
		// only the yyyy-mm-dd part for the `<input type="date">` control.
		return s.expires_at ? s.expires_at.slice(0, 10) : null;
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
				<!--
					Reveal-on-pick: at rest this is ONE full-width field, the only
					one that holds free text. Role, expiry and [Add] appear once a
					recipient is chosen, which is the first moment any of them
					means anything — before that [Add] was merely greyed and the
					role was a choice about nobody. It also keeps the form short
					while you are still typing a name, so the member list below
					stays in view.
				-->
				<div class="add-row">
					<div class="add-row__line">
						<div class="search">
							<!--
							The picks and the search box, TOGETHER. Picking used to
							swap the field out for the chosen chip, which meant the
							second name could not be typed until the first had been
							granted: one round trip, one role press and one [Add] per
							person. They now queue here and the field stays, so a
							handful of people at one access level is one [Add].

							`share-dialog-selected` stays on the queue container —
							three e2e specs assert it appears after picking a result.
						-->
							{#if picks.length > 0}
								<div class="sh-picks" data-testid="share-dialog-selected">
									{#each picks as p (p.type + p.id)}
										<span class="sh-picked" data-testid={`share-dialog-pick-${p.type}-${p.id}`}>
											<Icon
												name={p.type === 'group'
													? 'user-group'
													: p.type === 'email'
														? 'envelope'
														: 'user'}
											/>
											<span class="sh-picked__label">{p.label}</span>
											<button
												class="btn-action"
												data-testid={`share-dialog-unpick-${p.type}-${p.id}`}
												title={t('share.remove_pick', 'Remove from the list')}
												onclick={() => unpick(p)}><Icon name="times" /></button
											>
										</span>
									{/each}
								</div>
							{/if}
							<input
								bind:this={searchInput}
								data-testid="share-dialog-search-input"
								placeholder={picks.length > 0
									? t('share.add_someone_else', 'Add someone else…')
									: t('share.add_people', 'Add people, groups, or email…')}
								bind:value={query}
								oninput={onQueryInput}
								autocomplete="off"
							/>
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
					</div>
					{#if picks.length > 0}
						<!--
							Three visible choices rather than a dropdown: the role is
							the consequential decision here, and a `<select>` hid all
							three behind the one it happened to default to. Same
							radiogroup shape as the appearance switcher in the user
							menu, worded rather than icon-only — an icon alone cannot
							distinguish "can edit" from "can manage" to someone who
							has not learned the glyphs.

							Ascending order (view → edit → manage) so the safest
							choice is first and reads left to right; `ROLES` itself is
							ordered the other way for the member list's sort.
						-->
						<div class="role-choice" role="radiogroup" aria-label={t('share.role_label', 'Role')}>
							{#each roleChoices as r (r.v)}
								<button
									type="button"
									class="role-choice__opt"
									class:role-choice__opt--active={newRole === r.v}
									role="radio"
									aria-checked={newRole === r.v}
									data-testid={`share-dialog-role-${r.v}-btn`}
									onclick={() => (newRole = r.v)}
								>
									<Icon name={r.icon} />
									<span>{r.l}</span>
								</button>
							{/each}
						</div>
						<div class="add-row__line add-row__line--commit">
							{@render expiryChip(newExpiry, (v) => (newExpiry = v))}
							<!--
								The commit. Role and expiry are only meaningful
								because this exists: while picking a result granted
								access outright, they had to be set before the name
								was typed, so nobody did.

								Still immediate — the dialog gains no Save. Its own
								single button stays [Close], and this grants the whole
								queue at the role above. `disabled` keeps the busy
								guard; the empty-queue half is now also structural,
								since this button only exists once someone is picked.

								The count is in the label once there is more than one
								to grant: [Add] alone, over a queue of five chips,
								reads as "add the one I just typed".
							-->
							<button
								class="btn btn-primary add-row__submit"
								data-testid="share-dialog-add-btn"
								disabled={picks.length === 0 || addGrantAction.busy}
								onclick={addSelected}
							>
								{#if addGrantAction.busy}
									{t('share.adding', 'Adding…')}
								{:else if picks.length > 1}
									{t('share.add_n', { n: picks.length }, 'Add {{n}}')}
								{:else}
									{t('share.add', 'Add')}
								{/if}
							</button>
						</div>
					{/if}
				</div>
				{#if addGrantAction.error}
					<p class="form-error" role="alert">{addGrantAction.error}</p>
				{/if}
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
					<label>
						<span>{t('share.download_limit', 'Download limit (optional)')}</span>
						<input
							type="number"
							min="1"
							step="1"
							inputmode="numeric"
							placeholder={t('share.download_limit_placeholder', 'Unlimited')}
							data-testid="share-dialog-link-downloads-input"
							value={maxDownloads ?? ''}
							oninput={(e) => {
								const raw = e.currentTarget.value.trim();
								const n = Number(raw);
								maxDownloads =
									raw === '' || !Number.isFinite(n) ? null : Math.max(1, Math.trunc(n));
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
					disabled={createLinkAction.busy || linkBlockedReason !== null}
					onclick={createLink}
					aria-describedby={linkBlockedReason || linkPolicyHint ? 'share-link-policy' : undefined}
					title={linkBlockedReason ?? undefined}
				>
					{createLinkAction.busy
						? t('common.loading', 'Loading…')
						: t('share.create_link', 'Create link')}
				</button>
				{#if createLinkAction.error}
					<p class="form-error" role="alert">{createLinkAction.error}</p>
				{/if}
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
							{#if s.max_downloads !== null}
								<span
									class="link-row__expiry"
									title={t('share.download_limit', 'Download limit (optional)')}
								>
									{t(
										'share.downloads_used',
										{ used: s.download_count, max: s.max_downloads },
										'{{used}} / {{max}} downloads'
									)}
								</span>
							{/if}
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
		flex-direction: column;
		gap: var(--space-2);
		padding-bottom: var(--space-4);
		border-bottom: 1px solid var(--color-border);
		margin-bottom: var(--space-4);
	}

	/* Two fixed lines rather than one wrapping row: the name field keeps the
	   first line (minus the role select) whatever the dialog's width, instead
	   of being the flex item that gives up its width to three controls that
	   each have an intrinsic one. */
	.add-row__line {
		display: flex;
		gap: var(--space-2);
		align-items: center;
	}

	/* Expiry at the left, [Add] at the right edge — the commit sits where a
	   dialog's primary action is looked for. */
	.add-row__line--commit {
		justify-content: space-between;
	}

	/*
	 * Role picker, same pill-on-a-muted-track shape as the appearance
	 * switcher in the user menu (`ported/userMenu.css::.theme-segmented`),
	 * but worded and full width: three equal segments that divide the row,
	 * so the control does not jump when the chosen label is longer than the
	 * others.
	 */
	.role-choice {
		display: flex;
		gap: var(--space-0-5);
		padding: var(--space-0-5);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-full);
		background: var(--color-bg-muted);
	}

	.role-choice__opt {
		display: inline-flex;
		flex: 1;
		gap: var(--space-2);
		align-items: center;
		justify-content: center;
		padding: var(--space-2) var(--space-3);
		border: none;
		border-radius: var(--radius-full);
		background: transparent;
		color: var(--color-text-subtle);
		font-size: var(--text-sm);
		cursor: pointer;
		transition:
			background 0.15s ease,
			color 0.15s ease;
	}

	.role-choice__opt:hover {
		color: var(--color-text);
	}

	.role-choice__opt--active {
		background: var(--color-accent);
		color: var(--color-on-accent);
	}

	.role-choice__opt--active:hover {
		color: var(--color-on-accent);
	}

	.add-row__submit {
		flex: none;
	}

	.search {
		position: relative;
		flex: 1;
		min-width: 0;
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
	/* The queue, above the field it was typed into. Wraps rather than
	   scrolls: five names is a plausible share and a hidden chip is a person
	   about to be granted access unnoticed. */
	.sh-picks {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-1);
		margin-bottom: var(--space-1);
	}

	/* One queued recipient. Pill-shaped and compact, so a row of them does
	   not read as a row of inputs — they are a selection, not fields. */
	.sh-picked {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		max-width: 100%;
		min-width: 0;
		padding: var(--space-0-5) var(--space-1) var(--space-0-5) var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-full);
		background: var(--color-bg-surface);
		font-size: var(--text-sm);
	}

	.sh-picked__label {
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

	/* Full-width commit action under the fields. */
	.sh-create__submit {
		width: 100%;
		margin-top: var(--space-4);
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
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: var(--space-3);
	}

	/*
	 * `justify-content: end` so the INPUTS line up across a row, not the
	 * labels above them.
	 *
	 * These cells stretch to the tallest in their grid row, and the label
	 * text is what varies: "Download limit (optional)" wraps to two lines at
	 * dialog width while "Expires (required)" does not. With the content
	 * packed to the top, each input sat directly under its own caption, so
	 * two fields side by side had their boxes at different heights and the
	 * row read as misaligned. Pushing each stack to the bottom of its cell
	 * puts every input on one line and lets the caption take whatever room
	 * it needs above.
	 */
	.sh-fields label {
		display: flex;
		flex-direction: column;
		justify-content: end;
		gap: 0.25rem;
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

	.btn-action--delete {
		color: var(--color-danger-alt);
	}

	.btn-action--delete:hover {
		color: var(--color-danger-alt);
		background: var(--color-danger-light-bg);
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
		padding: var(--space-2) var(--space-2);
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
