/**
 * API wire types — ported from static/js/core/types.js.
 *
 * This is a focused, hand-ported subset covering the core resources. The plan
 * is to regenerate the full set from the backend OpenAPI (`just openapi` +
 * `openapi-typescript`) so these track the Rust DTOs; until then, extend here.
 */

export type ItemType = 'file' | 'folder';

export interface LightItem {
	id: string;
	name: string;
	type: ItemType;
	parentId: string;
}

export interface FolderItem {
	category: string;
	created_at: number;
	icon_class: string;
	icon_special_class: string;
	id: string;
	is_root: boolean;
	modified_at: number;
	name: string;
	// §14 provenance — who originally created the folder. `null` when
	// the creating user has since been deleted (backend FK is
	// `ON DELETE SET NULL`), or when the folder is returned to a
	// share recipient that lost provenance via
	// `FolderDto::without_hierarchy_info`. The canonical "owner"
	// signal on the Files browser / Favorites / Shared surfaces
	// (replaced the retired `owner_id` field in D7).
	created_by: string | null;
	// §14 provenance — who last touched the folder (rename / move /
	// metadata change). The canonical "who touched this recently"
	// signal on the Recent surface.
	updated_by: string | null;
	parent_id: string | null;
	path: string;
	etag: string;
	/**
	 * The drive this folder belongs to (post-D0 ownership pivot per
	 * `docs/plan/drive.md` §3). Populated by the backend `FolderDto`
	 * on every response; the field was left out of the TS type until
	 * a caller needed it. Used by `/files` to resolve the current
	 * drive for the read-only banner without depending on the URL's
	 * leading segment being a drive-root folder id.
	 */
	drive_id: string;
	/**
	 * Caller-scoped: `true` when the requesting user has favorited
	 * this folder. Always present on the wire — never null, never
	 * absent — per the backend enrichment contract. `ResourceList`
	 * renders the fav-star chip natively from this field.
	 */
	is_favorite: boolean;
	/**
	 * Resource-scoped: `true` when the folder has any
	 * `storage.role_grants` entry (link share via `subject_type =
	 * 'token'`, user grant, group grant, any role). "Someone was
	 * given access to this beyond drive membership." Always present
	 * on the wire.
	 */
	is_shared: boolean;
}

export interface FileItem {
	category: string;
	created_at: number;
	icon_class: string;
	icon_special_class: string;
	id: string;
	mime_type: string;
	modified_at: number;
	name: string;
	// §14 provenance — see FolderItem for semantics. Replaced the
	// retired `owner_id` field in D7.
	created_by: string | null;
	updated_by: string | null;
	folder_id: string;
	path: string;
	size: number;
	size_formatted: string;
	sort_date: number;
	etag: string;
	content_hash: string;
	/** See `FolderItem.is_favorite` — same wire contract. */
	is_favorite: boolean;
	/** See `FolderItem.is_shared` — same wire contract. */
	is_shared: boolean;
	/** Search-only: plain-text fragment around a content match. */
	snippet?: string;
	/** Search-only: "name" or "content". */
	match_source?: string;
}

export interface ShareItem {
	access_count: number;
	created_at: number;
	created_by: string;
	expires_at: number;
	has_password: boolean;
	id: string;
	item_id: string;
	item_name: string;
	item_type: ItemType;
	token: string | null;
	url: string;
}

export interface CreateShare {
	item_id: string;
	item_name?: string | null;
	item_type: ItemType;
	password: string | null;
	expires_at: number | null;
}

export interface UpdateShare {
	password?: string | null;
	expires_at?: number | null;
}

export interface FavoriteItem {
	id: string;
	user_id: string;
	item_id: string;
	item_type: ItemType;
	created_at: number;
	item_name: string | null;
	item_size: number | null;
	item_mime_type: string | null;
	parent_id: string | null;
	modified_at: number | null;
	item_path: string;
	icon_class: string;
	icon_special_class: string;
	category: string;
	size_formatted: string;
}

export interface RecentItem {
	id: string;
	user_id: string;
	item_id: string;
	item_type: ItemType;
	accessed_at: number;
	item_name: string | null;
	item_size: number | null;
	item_mime_type: string | null;
	parent_id: string | null;
	item_path: string;
	icon_class: string;
	icon_special_class: string;
	category: string;
	size_formatted: string;
}

export interface TrashResourceItem {
	resource_type: ItemType;
	trashed_at: string;
	deletion_date: string;
	/**
	 * Drive the trashed item belongs to (D2b). Enables client-side
	 * group-by-drive in the `/trash` UI without resolving the drive from
	 * `resource.drive_id` per row. The drive's display name resolves
	 * against `drives.svelte` (the in-memory store already populated by
	 * the sidebar picker / config pages — no extra round-trip).
	 */
	drive_id: string;
	resource: FileItem | FolderItem;
}

export interface TrashResourcesResponse {
	items: TrashResourceItem[];
	next_cursor?: string;
}

export type Role = 'user' | 'admin';

/** Wire shape of `UserDto` (backend: src/application/dtos/user_dto.rs). */
export interface User {
	id: string;
	username?: string;
	email: string;
	role: string;
	storage_quota_bytes: number;
	storage_used_bytes: number;
	created_at: string;
	updated_at: string;
	last_login_at?: string | null;
	active: boolean;
	auth_provider: string;
	image?: string | null;
	can_edit_image: boolean;
	is_external: boolean;
	given_name?: string;
	family_name?: string;
	email_verified_at?: string;
	preferred_locale?: string;
	notify_on_share: boolean;
	/**
	 * Opaque UI preferences bag. Server-side JSONB column that persists
	 * pure UI toggles (hide-dotfiles, view mode, sidebar collapse, …)
	 * across devices. The server never inspects the contents — the SPA
	 * defines the keys (see `lib/stores/preferences.svelte.ts` for the
	 * typed view). Always an object on the wire (empty bag is `{}`,
	 * never `null` or missing).
	 *
	 * When PATCHing back to the server via
	 * `PATCH /api/auth/me/profile { ui_preferences: {...} }`, the
	 * server SHALLOW-merges — only the keys present in the patch are
	 * touched, so partial writes from one device don't clobber
	 * preferences set on another. Set a key to `null` in the patch to
	 * delete it from the bag.
	 */
	ui_preferences: Record<string, unknown>;
	/**
	 * Mirrors `auth.users.force_password_change_at_next_login`. Only
	 * populated by `GET /api/auth/me` (see the backend UserDto doc for
	 * why other UserDto call-sites default to false). When true, the
	 * SPA MUST lock navigation to the password-change surface — the
	 * root layout's guard + the backend's `require_no_password_change_pending`
	 * middleware together enforce this. Optional on the wire because
	 * older backend builds omit it and `#[serde(default)]` maps
	 * missing → `false`.
	 */
	force_password_change?: boolean;
	/**
	 * TRUE when the account has a local Argon2id `password_hash` on
	 * file. Distinct from `auth_provider`: an SSO-linked account can
	 * ALSO carry a local password (hybrid posture — SSO for daily
	 * login, local password as fallback). The profile page's
	 * change-password card gates on this flag rather than on
	 * `auth_provider === 'local'` so hybrid users can rotate their
	 * local credential. Optional on the wire for older-backend
	 * compatibility; missing → `false` (safe default: hide the card).
	 */
	has_password?: boolean;
}

/** Fields rendered by the paginated admin table. Full account details remain
 * available from the detail endpoint; this shape keeps avatars and preference
 * documents off every listing page.
 *
 * The two OPAQUE flags below are ADMIN-ONLY signals: they surface per-user
 * OPAQUE rollout progress in the admin table. The backend deliberately keeps
 * them off `UserDto` (`/api/auth/me`, share-recipient DTOs, group members)
 * so a non-admin can't enumerate the adoption set through third-party
 * endpoints. Both optional on the wire — older backend builds omit them and
 * `#[serde(default)]` maps missing → `false`. */
export type AdminUserSummary = Pick<
	User,
	| 'id'
	| 'username'
	| 'email'
	| 'role'
	| 'storage_quota_bytes'
	| 'storage_used_bytes'
	| 'last_login_at'
	| 'active'
	| 'auth_provider'
	| 'is_external'
> & {
	/** TRUE = user has a server-verifiable password on file (legacy or
	 * admin-set). Combined with `opaque_registered` and `auth_provider`,
	 * the admin table derives the full auth capability set — a user with
	 * `has_password=false`, `opaque_registered=false` AND
	 * `auth_provider === 'local'` is passwordless (magic-link only,
	 * which is the default for externals). */
	has_password?: boolean;
	/** TRUE = user has an OPAQUE envelope on file (Phase 2 silent migration
	 * succeeded, or the user completed a manual re-registration). */
	opaque_registered?: boolean;
	/** TRUE = user has completed at least one successful OPAQUE login.
	 * Distinct from `opaque_registered` — the envelope may have been
	 * cleared by an admin reset while a stale migrated=true remains as
	 * historical signal (backend clears both atomically today, but the
	 * two-flag shape keeps the option open for a future policy split). */
	opaque_migrated?: boolean;
};

export interface AdminUsersPage {
	total: number;
	users: AdminUserSummary[];
}

export interface AuthResponse {
	user: User;
	access_token: string;
	refresh_token: string;
	token_type: string;
	expires_in: number;
	/**
	 * Mirrors `auth.users.force_password_change_at_next_login` — set
	 * TRUE by the admin password-reset flow (see backend
	 * `OpaquePgRepository::clear_registration`) so admin-picked
	 * passwords stay temporary until the user changes them. When true,
	 * the SPA's post-login handler must route to `/settings/security`
	 * (or the equivalent change-password surface) instead of the
	 * user's home. Cleared server-side by a successful
	 * `POST /api/auth/change-password`.
	 *
	 * Optional on the wire because the backend `#[serde(default)]`s
	 * to `false` — older clients / non-login endpoints hitting this
	 * type won't nil-deref.
	 */
	force_password_change?: boolean;
}

/**
 * Sort dimension for `GET /api/search`. Wire-matches the backend's
 * `SearchResourcesQuery.order_by` — 5 canonical values, direction is
 * a separate `reverse` boolean (the `_desc` suffix pattern was
 * retired 2026-07-26; `date` was renamed to the more explicit
 * `updated_at` alongside the new `created_at`).
 */
export type SortBy = 'relevance' | 'name' | 'size' | 'updated_at' | 'created_at';

/**
 * Per-item search metadata inline on every hit in the normalized
 * `/api/search` envelope. Mirrors backend `SearchMeta` — see
 * `application/dtos/search_dto.rs`.
 */
export interface SearchMeta {
	/** Relevance in [0, 1]; the higher the better. */
	score: number;
	/** Optional HTML-safe excerpt when the match fired via content index. */
	snippet?: string;
	/** Where the match fired. */
	via?: 'name' | 'content' | 'path';
}

/**
 * Single hit in the `/api/search` envelope. `resource_type` disambiguates
 * `resource`'s union so the shared `ResourceList` component can render it
 * exactly like a folders/favorites/recent/trash row.
 */
export interface SearchResourceItem {
	resource_type: ItemType;
	resource: FileItem | FolderItem;
	meta: SearchMeta;
}

/**
 * Wire response of `GET /api/search`. Same envelope shape as the other
 * "resources" listing endpoints (`items[]` + optional `next_cursor`),
 * plus two search-specific top-level fields: `query_time_ms` (health
 * signal for admins, "Found N in Xms" for users) and `total` (approximate,
 * caller-visible; never leaks a count for rows the caller can't see).
 */
export interface SearchResourcesResponse {
	items: SearchResourceItem[];
	next_cursor?: string;
	query_time_ms: number;
	total?: number;
}

export type DriveKind = 'personal' | 'shared';

/**
 * Full role set from `storage.grant_role` — every value that can appear
 * on a `role_grants` row regardless of `resource_type` (drive, folder,
 * file, playlist, calendar, address_book, …). Use this for folder-level
 * and file-level `caller_role` fields where all five values are valid.
 * Matches `RoleDto` in the backend.
 */
export type GrantRole = 'owner' | 'editor' | 'contributor' | 'commenter' | 'viewer';

/**
 * Role assignable at DRIVE scope — a strict subset of `GrantRole`.
 * Drives only meaningfully take the three management-ladder tiers:
 * - `owner`  — full control (rename, delete, quota, membership).
 * - `editor` — can create/modify content anywhere in the drive.
 * - `viewer` — read-only access to the whole drive.
 *
 * `contributor` (create-in-folder-without-touching-siblings) and
 * `commenter` (react without modifying) are folder/file-scope
 * semantics: they describe fine-grained access to a specific item,
 * not to a whole drive. Grants of those roles happen at folder or
 * file scope via a separate `role_grants` row, not at the drive
 * boundary. Do NOT widen this type without a matching backend
 * check — the DB ENUM permits all 5 today, so the constraint is
 * conventional.
 *
 * Use `GrantRole` for folder/file-level `caller_role` fields.
 */
export type DriveRole = 'owner' | 'editor' | 'viewer';

/** Subject of a grant. Mirrors `SubjectDto`. */
export type SubjectKind = 'user' | 'group' | 'token';
export interface DriveMemberSubject {
	type: SubjectKind;
	id: string;
}

/**
 * One row from `GET /api/drives`. Mirrors `DriveDto` in
 * `src/application/dtos/drive_dto.rs`. `default_for_user` is the caller's
 * id when present, `null`/undefined otherwise — used to pick the default
 * personal drive without hard-coding name conventions.
 *
 * `caller_role` is the strongest role the calling user holds on this drive
 * (direct + group-mediated, collapsed). Drives the permission-aware UI
 * gating on `/config/drive/<id>` and similar pages. `undefined` in
 * contexts where the caller is the granter rather than a member (e.g.
 * outgoing-grants listing).
 */
export interface Drive {
	id: string;
	name: string;
	kind: DriveKind;
	default_for_user?: string | null;
	root_folder_id: string;
	quota_bytes?: number | null;
	used_bytes: number;
	/**
	 * Drive policies — raw JSONB bag from the backend. Unknown keys are
	 * preserved verbatim. For the typed view used by the admin policy
	 * editor, see [`DrivePolicies`].
	 */
	policies: Record<string, unknown>;
	created_at: string;
	updated_at: string;
	caller_role?: DriveRole | null;
}

/**
 * Typed mirror of the known drive policy keys. Every field defaults to
 * `false` (= "opted out" for the `include_in_*` keys, "allowed" for the
 * `forbid_*` keys). The wire shape returned by
 * `PATCH /api/drives/{id}/policies` carries every known key; the request
 * body uses [`DrivePoliciesPartial`] so unsupplied keys aren't disturbed
 * (the backend uses a JSONB `||` merge — see
 * `drive_pg_repository.rs::update_policies`).
 *
 * See `docs/plan/drive.md` §8 for the `forbid_*` gates and §15 for the
 * `include_in_*_index` scope flags.
 */
export interface DrivePolicies {
	forbid_sharing: boolean;
	forbid_external_sharing: boolean;
	forbid_public_links: boolean;
	forbid_cross_drive_move: boolean;
	forbid_owner_role_change: boolean;
	/**
	 * §15 opt-in for `/api/photos` timeline scope. Default personal drives
	 * are created with `true`; non-default drives (secondary personals,
	 * shared) start `false` and opt in via the admin policy modal.
	 */
	include_in_photo_index: boolean;
	/**
	 * §15 opt-in for the Music library surface (currently playlists;
	 * future `/api/music/tracks` library view will read this too).
	 * Symmetric shape to `include_in_photo_index`.
	 */
	include_in_music_index: boolean;
	/**
	 * Full freeze / legal-hold. When `true`, every mutation on resources
	 * in the drive is refused — user-initiated AND background alike (the
	 * trash-retention purge SQL filter excludes read-only drives). Only
	 * `Read` passes. Admins can un-freeze via the admin-only policy PATCH.
	 * See `docs/plan/drive.md` §8 (`read_only`).
	 */
	read_only: boolean;
}

/**
 * Body shape for the admin policy editor — every key optional so omitting
 * a field leaves that policy untouched (the backend uses a JSONB merge).
 */
export type DrivePoliciesPartial = Partial<DrivePolicies>;

/**
 * Request body for `POST /api/drives` (D3a). Mirrors `CreateDriveDto` in
 * `src/interfaces/api/handlers/drive_handler.rs`. `kind: 'personal'` is a
 * recognised wire shape but returns 501 today (the authz model + quota
 * source for secondary personals are still open product questions).
 */
export interface CreateDriveBody {
	kind: DriveKind;
	name: string;
	owner: DriveMemberSubject;
	quota_bytes?: number | null;
}

/**
 * One row from `GET /api/drives/{id}/members`. Mirrors `GrantDto` in
 * `src/application/dtos/grant_dto.rs` — the shape is the same as any
 * other role-grant; drive membership just constrains `resource.type` to
 * `"drive"`.
 */
export interface DriveMember {
	id: string;
	subject: DriveMemberSubject;
	resource: { type: 'drive'; id: string };
	role: DriveRole;
	granted_by: string;
	granted_at: string;
	expires_at?: string | null;
}

// ─── Folder ancestors (breadcrumb endpoint) ──────────────────────────────
// Wire shape of `GET /api/folders/{id}/ancestors`. Mirrors the backend
// `FolderAncestorsDto` — see `src/application/dtos/folder_dto.rs`. One
// round-trip returns the whole caller-visible parent chain plus an
// `access_source` telling the breadcrumb component which root icon /
// tooltip to render.

export interface FolderAncestor {
	id: string;
	name: string;
	/** `null` on the drive-root ancestor. */
	parent_id: string | null;
	/**
	 * Drive the folder belongs to (always populated — every folder has a
	 * drive_id post-D0). Lets `/files` derive `currentFolderDriveId` from
	 * the ancestors response instead of firing an extra
	 * `GET /api/folders/{id}` on load. Same value across every entry in
	 * `ancestors` (all folders in a chain live in one drive).
	 */
	drive_id: string;
}

/**
 * How the caller reached the topmost accessible ancestor.
 * - `drive` — via drive membership (own personal, secondary personal, or
 *   shared drive). `drive` field carries the drive's id/name/kind for
 *   the root icon.
 * - `direct_share` — via a folder-level `role_grants` row (share).
 *   `subject` may name the grantee (self or a group) once subject
 *   enrichment lands; MVP leaves it null.
 * - `token` — reserved for public-link callers. Not emitted today.
 */
export type AccessSourceKind = 'drive' | 'direct_share' | 'token';

export interface AccessSourceDrive {
	id: string;
	name: string;
	kind: DriveKind;
}

export interface AccessSourceSubject {
	kind: 'user' | 'group';
	id: string;
	/** Nullable in MVP (subject enrichment deferred). */
	name?: string | null;
}

export interface AccessSource {
	kind: AccessSourceKind;
	/** Populated when `kind === 'drive'`. */
	drive?: AccessSourceDrive;
	/**
	 * SHARER — the user who created the grant that gave the caller
	 * access at the boundary (`role_grants.granted_by`). Kind is always
	 * `'user'` today (a group can't perform an action), but the type
	 * stays open in case a future model permits it. Null when the
	 * boundary can't be resolved to a single grant (e.g. `token`).
	 */
	subject?: AccessSourceSubject;
	/**
	 * Caller's role via the boundary grant (`role_grants.role` on the
	 * same row that carries `granted_by`). Lets the FE render permission-
	 * aware affordances at the ancestor scope. Reflects the boundary grant
	 * only — aggregate effective role via other channels may be stronger.
	 * Null on `token` access.
	 *
	 * Typed as `GrantRole` (not `DriveRole`): the boundary can be a
	 * folder-level share where all five role_grant values are valid,
	 * not just the drive-scoped subset.
	 */
	caller_role?: GrantRole | null;
}

/**
 * Response envelope of `GET /api/folders/{id}/ancestors`. `ancestors`
 * is root-first, leaf-last (length ≥ 1). `access_source` describes
 * the boundary at element 0 (drive root or share boundary).
 */
export interface FolderAncestorsResponse {
	ancestors: FolderAncestor[];
	access_source: AccessSource;
}

// ─── Job registry (Part 1 + Part 2) ────────────────────────────────────────
//
// Maps `src/infrastructure/scheduler/*` DTOs 1:1. See
// `docs/plan/job-registry.md` for the backend contract; the shapes below
// are what the `/api/admin/jobs*` endpoints emit.

/**
 * `JobOutcome` — the uniform outcome the scheduler logs and stores for
 * every job dispatch. Serialised with `#[serde(tag = "outcome")]` so the
 * discriminant is the `outcome` field, not the object key.
 */
export type JobOutcome =
	| { outcome: 'ok'; count: number; extra?: unknown }
	| { outcome: 'err'; message: string };

/**
 * `JobSummary` — one row per registered job in `GET /api/admin/jobs`.
 * Cadence + last-run bookkeeping. `interval_ms` / `next_run_at` are
 * `undefined` on on-demand jobs (serde skips `Option::None`).
 */
/**
 * Enough info about a paused recoverable run for the admin panel to
 * render "Resume (scanned/total)" on the job row without opening the
 * drawer. Absent when no `Paused` row exists for this job. `total`
 * is absent when the tenant didn't seed a countable subject —
 * fallback UI is just "Resume".
 */
export interface PausedRunBrief {
	id: string;
	scanned: number;
	total?: number;
}

export interface JobSummary {
	name: string;
	interval_ms?: number;
	next_run_at?: string;
	last_run_at?: string;
	last_outcome?: JobOutcome;
	running: boolean;
	/**
	 * `true` iff the job persists runs + findings to
	 * `jobs.recoverable_runs`. Consumed by the admin panel to decide
	 * whether the row is expandable (drawer with run history +
	 * findings) and to gate the retention/purge action — replaces
	 * the pre-K3 name-based allowlist that missed newly-added
	 * recoverable tenants (`backend_rotate` shipped first without a
	 * row-expand until this flag was added).
	 */
	recoverable: boolean;
	/** Populated iff a `Paused` row exists in `jobs.recoverable_runs`
	 *  for this job. Distinct from `running` — a paused run is
	 *  resumable via the same trigger endpoint. */
	paused_run?: PausedRunBrief;
}

/**
 * `RunStatus` values allowed in `jobs.recoverable_runs.status`. The
 * non-terminal set (Running / Paused / CancelRequested) is what the
 * DB's `one_active_run_per_job` partial unique index scopes.
 */
export type RunStatus =
	| 'Running'
	| 'Paused'
	| 'CancelRequested'
	| 'Completed'
	| 'Failed'
	| 'Cancelled';

/**
 * `RunSummary` — one row per recoverable-job run from
 * `GET /api/admin/jobs/{name}/runs`. Terminal + non-terminal rows both
 * appear. `stats` / `params` are opaque JSON — job-specific shape;
 * consumers should key off `job_name` to decide what to render.
 * `cursor_hex` is present only when the run has advanced past the
 * initial state (paused mid-scan is the typical case).
 */
export interface RunSummary {
	id: string;
	job_name: string;
	status: RunStatus;
	started_at: string;
	last_progress_at: string;
	completed_at?: string;
	stats: Record<string, unknown>;
	params: Record<string, unknown>;
	cursor_hex?: string;
	error_message?: string;
	/** Populated when the tenant reported a countable subject at run
	 *  start (`RecoverableJobHandler::count_total`). Absent when the
	 *  tenant can't count — the UI hides the progress bar and falls
	 *  back to raw `scanned_count`. */
	progress?: RunProgress;
}

/**
 * Confidence level of a `RunProgress` fraction. Wire lowercase per
 * the `#[serde(rename_all = "lowercase")]` on the Rust enum.
 *
 * - `count` — `scanned_count / total_rows` where `total_rows` came
 *   from a definitive `COUNT(*)` on the subject table.
 * - `approximate` — proxy-derived total (e.g. `storage_consistency`
 *   using DB blob count as a stand-in for backend object count).
 *   Fraction can legitimately exceed 1.0 at run end — the deviation
 *   quantifies the drift the check is looking for.
 */
export type ProgressKind = 'count' | 'approximate';

export interface RunProgress {
	fraction: number;
	kind: ProgressKind;
	scanned: number;
	total: number;
}

/**
 * `Finding` — one row from `GET /api/admin/jobs/{name}/runs/{id}/findings`.
 * Persisted by consistency tenants via `store.record_finding()`. Consumers
 * key off `kind` to know the shape of `detail` (per-tenant JSON — e.g.
 * `stale_used_bytes` carries `{cached, actual, delta}`; `missing_blob`
 * carries `{blob_hash}`; …).
 */
export interface Finding {
	id: string;
	run_id: string;
	kind: string;
	severity: string;
	resource_id?: string;
	detail: Record<string, unknown>;
	created_at: string;
}
