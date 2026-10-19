/**
 * API wire types — ported from static/js/core/types.js.
 *
 * This is the legacy, hand-ported subset used by endpoint modules that have not
 * migrated to the Hey API client yet. New generated types and SDK functions
 * live in `./generated`; refresh them from the Rust OpenAPI with
 * `just api-client`.
 */

// `import type`, not a value import: `endpoints/grants.ts` already pulls
// `ItemType` from this file, so a value import would close a runtime
// cycle. Type-only imports are erased entirely, leaving nothing to cycle.
import type { Grant } from './endpoints/grants';

export type ItemType = 'file' | 'folder';

export interface LightItem {
	id: string;
	name: string;
	type: ItemType;
	parentId: string;
}

// Authoritative on the generated SDK. `FolderDto` carries the same
// 15 fields with matching types (all primitives — no cascading
// hand-written type references), so the alias is pure name-forward
// with zero shape change. §Phase 3 of the generated-types
// normalisation. Local import so other hand-written interfaces in
// this file can keep referencing `FolderItem`.
import type { FolderDto as FolderItem } from './generated/openapi/types.gen';
export type { FolderItem };

export interface FileItem {
	category: string;
	/** RFC 3339 UTC timestamp (wire form after the DTO ISO-8601 pivot). */
	created_at: string;
	icon_class: string;
	icon_special_class: string;
	id: string;
	mime_type: string;
	/** RFC 3339 UTC timestamp. */
	modified_at: string;
	name: string;
	// §14 provenance — see FolderItem for semantics. Replaced the
	// retired `owner_id` field in D7.
	created_by: string | null;
	updated_by: string | null;
	folder_id: string;
	path: string;
	size: number;
	size_formatted: string;
	/**
	 * Deprecated on `FileItem` — `sort_date` moved to the item-level
	 * envelope on `/api/photos/resources` (§1 of
	 * `docs/plan/photos-resources-migration.md`), where `PhotoItem`
	 * declares it as a required RFC 3339 string. The field was only ever
	 * populated here by the legacy `/api/photos` route, which §4
	 * removed.
	 *
	 * Typed `string | number` solely so the two can coexist: `PhotoItem
	 * extends FileItem` must narrow to something assignable to this, and
	 * existing test fixtures and benches seed a bare number. The wire
	 * only ever sends the string — see {@link photoTimestamp}, which
	 * accepts both for that reason and prefers neither silently.
	 */
	sort_date?: string | number;
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
	/** RFC 3339 UTC timestamp. */
	created_at: string;
	created_by: string;
	/** RFC 3339 UTC timestamp; `null` → never expires. */
	expires_at: string | null;
	has_password: boolean;
	id: string;
	item_id: string;
	item_name: string;
	item_type: ItemType;
	token: string | null;
	url: string;
}

// Authoritative on the generated SDK — Group B of the Phase 3
// rename-pattern sweep. The handwritten shape diverged in two
// benign ways: `item_type: ItemType` ('file' | 'folder') widens
// to `item_type: string`, and `password` / `expires_at` move from
// `: string | null` (required nullable) to `?: string | null`
// (optional nullable). Both are strict widenings on the input side,
// so every current caller passing a literal `'file'`/`'folder'`
// item_type or an explicit `null` for the other two still fits.
import type { CreateShareDto as CreateShare } from './generated/openapi/types.gen';
export type { CreateShare };

// Authoritative on the generated SDK. Identical shape, no
// cascading dependencies — pure name-forward.
export type { UpdateShareDto as UpdateShare } from './generated/openapi/types.gen';

// Authoritative on the generated SDK — Phase 3 Group D. Both FavoriteItem
// and RecentItem shared the exact same diff pattern: `item_type: ItemType`
// → `item_type: ItemKind` (same 2-value enum via the Rust-side `ItemKind`
// derive) + 5-6 required-nullable fields widen to optional-nullable
// (`item_name`, `item_size`, `item_mime_type`, `parent_id`, `item_path`,
// and on favorites also `modified_at`). Call-sites doing bare truthy
// checks on the nullables keep working; the handful reading `.item_path`
// unconditionally now narrow via `?.` which is already common idiom.
// Local import so `FavoritesResponse` / `RecentResponse` below keep
// resolving the short names.
import type {
	FavoriteItemDto as FavoriteItem,
	RecentItemDto as RecentItem
} from './generated/openapi/types.gen';
export type { FavoriteItem, RecentItem };

// Authoritative on the generated SDK — Phase 3 Group D. Two reshapes:
// `resource_type: ItemType` ('file'|'folder') widens to `ResourceTypeDto`
// (6-value union — all legitimate trash kinds are a strict subset so
// current `.resource_type` switches still cover reality), and
// `resource: FileItem | FolderItem` widens to `ResourceContentDto`
// (= FileDto | FolderDto | DriveDto). The display-enriched fields
// (`icon_class`, `category`, `size_formatted`, …) are present on both
// FileDto and FolderDto, so UI rows that read them keep working;
// DriveDto is a phantom branch — drives never go into trash, so a
// `kind === 'drive'` match is dead code at runtime but legal at the
// type level.
import type { TrashResourceItemDto as TrashResourceItem } from './generated/openapi/types.gen';
export type { TrashResourceItem };

export interface TrashResourcesResponse {
	items: TrashResourceItem[];
	next_cursor?: string;
}

// Authoritative on the generated SDK. The handwritten alias was a
// 2-value `'user' | 'admin'` union missing the `owner` row that
// `auth.users.role` has carried since the Owner role landed. The
// generated `UserRoleDto` is 3-value and matches reality.
export type { UserRoleDto as Role } from './generated/openapi/types.gen';

// ─────────────────────────────────────────────────────────────────────────
// Three-layer user family — mirrors src/application/dtos/user_dto.rs.
// See docs/plan/userdto-refactor.md.
//
// `PublicUser`  — public identity. Every authenticated caller may see it.
//                 Returned by /api/users/{id}, share responses, group
//                 members, magic-link invitees, recipient enrichment.
// `FullUser`    — `{ user: PublicUser, ...admin+self extras }`. Returned
//                 as Vec by /api/admin/users; embedded in `SelfUser`.
// `SelfUser`    — `{ full: FullUser, ...self-only extras }`. Returned by
//                 /api/auth/me and by every auth response.
//
// Adding a field? Decide by audience:
//   * Any authenticated caller may see it about another user → PublicUser.
//   * Only admin (about another user) AND self (about self) → FullUser.
//   * Only self about themselves → SelfUser.
// ─────────────────────────────────────────────────────────────────────────

// Authoritative on the generated SDK — Group B of the Phase 3
// rename-pattern sweep. The handwritten shape differed only in three
// optional fields (`username`, `given_name`, `family_name`) that the
// generated DTO types as `?: string | null` instead of `?: string`.
// Call-sites doing a bare truthy check on these keep working (null
// is falsy); the few that typed a variable off `.username` directly
// absorb a `| null` in the inferred type.
//
// Local import so other hand-written interfaces in this file
// (`FullUser.user: PublicUser`, `AccessSourceSubject` neighbours)
// keep resolving the name.
import type { PublicUserDto as PublicUser } from './generated/openapi/types.gen';
export type { PublicUser };

// Authoritative on the generated SDK — Phase 3 Group D. The handwritten
// shape differed in five cosmetic places: `user: PublicUser` cascades
// to `PublicUserDto` (already aliased), `federation_kind` becomes
// `FederationKindDto` (same 3 values, now null-tolerant), and three
// optional strings widen to `?: X | null`. All field docs are carried
// on the generated side via utoipa — see `src/application/dtos/user_dto.rs`.
import type { FullUserDto as FullUser } from './generated/openapi/types.gen';
export type { FullUser };

// Authoritative on the generated SDK — Group C of the Phase 3
// rename-pattern sweep. The handwritten shape differed only in
// `full: FullUser` (now resolves to `FullUserDto` — see sibling
// cascade below) and `ui_preferences: Record<string, unknown>` which
// the generated type widens to `unknown`. `Record<string, unknown>`
// is assignable INTO `unknown`, so every current consumer that
// writes-through or reads with narrowing still compiles.
import type { SelfUserDto as SelfUser } from './generated/openapi/types.gen';
export type { SelfUser };

/** Backwards-compat alias while migrating call-sites. Prefer `PublicUser`
 * for public-identity contexts (sharee, group member, invitee) or
 * `SelfUser` when reading `/api/auth/me`. Delete once no consumers reference
 * the bare `User` name. */
export type User = PublicUser;

export interface AdminUsersPage {
	total: number;
	users: FullUser[];
}

// Authoritative on the generated SDK. The handwritten shape only
// differed in `user: SelfUser` vs `user: SelfUserDto` — those two
// are structurally compatible (`SelfUser` differs only in one
// sibling-renamed field, see Group C of the Phase 3 reclassification).
// All field docs — including the detailed `force_password_change`
// explanation — are already carried by the generated type via utoipa.
export type { AuthResponseDto as AuthResponse } from './generated/openapi/types.gen';

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
// Shapes authoritative on the generated SDK — re-exported here so
// consumers importing from `$lib/api/types` keep working. The
// generated `SearchResourceItem.resource` is `ResourceContentDto`
// (a `FileDto | FolderDto | DriveDto` untagged union) rather than
// the pre-SDK `FileItem | FolderItem` hand-written union: a search
// result CAN now be a Drive. Consumers that discriminated on
// `resource_type` already handle the broader union correctly; those
// that pattern-matched on the two-arm shape need updating at the
// call site (TS surfaces the delta).
//
// `SearchResourcesResponse` maps to the generated `SearchResourcesDto`
// — the openapi-ts-emitted `SearchResourcesResponse` is a status-code
// wrapper (`{ 200: SearchResourcesDto }[keyof …]`), not the payload
// shape the FE wants.
export type { SearchMeta, SearchResourceItem } from './generated/openapi/types.gen';
export type { SearchResourcesDto as SearchResourcesResponse } from './generated/openapi/types.gen';

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
// Authoritative on the generated SDK — Group C of the Phase 3
// rename-pattern sweep. The handwritten shape differed in three
// cosmetic places: `kind: DriveKind` → `kind: DriveKindDto` (same
// 2-string union), `caller_role?: DriveRole | null` → `caller_role?:
// null | RoleDto` (the generated RoleDto carries all 5 role values;
// at a drive it will only ever be one of the drive-scoped subset in
// practice — the server narrows), and `policies: Record<string,
// unknown>` → `policies: unknown`. All three widen at the type level
// without changing wire semantics. Local import so other hand-written
// interfaces in this file (DriveSummary, DriveMembership) keep
// resolving `Drive` by its short name.
import type { DriveDto as Drive } from './generated/openapi/types.gen';
export type { Drive };

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
// Shape is authoritative on the generated SDK (every policy is
// optional there — a partial payload is a legal "patch" that leaves
// unlisted keys untouched). Re-exported under the same name for
// consumers importing from `$lib/api/types`; local import so the
// `DrivePoliciesPartial` alias below can keep its definition.
import type { DrivePolicies } from './generated/openapi/types.gen';
export type { DrivePolicies };

/**
 * Body shape for the admin policy editor — every key optional so omitting
 * a field leaves that policy untouched (the backend uses a JSONB merge).
 */
export type DrivePoliciesPartial = Partial<DrivePolicies>;

/**
 * What changing a per-kind default WOULD do, from
 * `PUT /api/admin/drive-policies/defaults/{kind}?dry_run=true`.
 */
// Authoritative on the generated SDK — Phase 3 Group D. The inline
// `by_knob` sub-shape and the handwritten `PolicyDrift` interface
// both get retired in favour of their generated siblings:
//
//   PolicyDrift              → WeakerDriveDto
//   { knob, follows, overrides } (inline) → PolicyKnobImpactDto
//
// `PolicyDrift` is kept as an alias for `WeakerDriveDto` because it's
// imported directly by `AdminDrivePoliciesPanel.svelte` and
// `endpoints/admin.ts` under that name. The one real widening is
// `kind: DriveKind` → `kind: string` on the generated side (Rust-side
// opportunity for a future enum tightening).
import type {
	PolicyDefaultsImpactDto as PolicyDefaultsImpact,
	WeakerDriveDto as PolicyDrift
} from './generated/openapi/types.gen';
export type { PolicyDefaultsImpact, PolicyDrift };

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

// Authoritative on the generated SDK. All four fields are
// primitives, no cascading dependencies — pure name-forward. The
// generated version types `parent_id?: string | null` where the
// pre-SDK hand-written was `parent_id: string | null` (always
// present): matches the server's actual emit (JSON `null` on the
// drive-root ancestor, not a dropped key). Local import so other
// hand-written interfaces in this file can keep referencing
// `FolderAncestor`.
import type { FolderAncestorDto as FolderAncestor } from './generated/openapi/types.gen';
export type { FolderAncestor };

/**
 * How the caller reached the topmost accessible ancestor.
 * - `drive` — via drive membership (own personal, secondary personal, or
 *   shared drive). `drive` field carries the drive's id/name/kind for
 *   the root icon.
 * - `direct_share` — via a folder-level `role_grants` row (share).
 *   `subject` may name the grantee (self or a group) once subject
 *   enrichment lands; MVP leaves it null.
 * - `token` — reserved for public-link callers. Not emitted today.
 *
 * Shape is authoritative on the generated SDK; local import so the
 * `AccessSource` interface below can keep its typed `kind` field,
 * plus a re-export so `$lib/api/types` consumers still resolve the
 * name here.
 */
import type { AccessSourceKind } from './generated/openapi/types.gen';
export type { AccessSourceKind };

// Authoritative on the generated SDK — Group A of the Phase 3
// rename-pattern sweep. The handwritten `AccessSourceDrive.kind`
// typed its enum as the handwritten `DriveKind`; the generated
// equivalent uses `DriveKindDto` (same string-union shape).
// `AccessSourceSubject.kind` was the inline literal `'user' | 'group'`;
// the generated version factors it out as `AccessSourceSubjectKind`
// (same two values).
//
// Import-then-export because the `AccessSource` interface below still
// references both names locally; a bare `export type { … as … }`
// doesn't bind the alias into module scope.
import type {
	AccessSourceDriveDto as AccessSourceDrive,
	AccessSourceSubjectDto as AccessSourceSubject
} from './generated/openapi/types.gen';
export type { AccessSourceDrive, AccessSourceSubject };

// Authoritative on the generated SDK — Group C of the Phase 3
// rename-pattern sweep. The handwritten shape differed only in
// three nullable-optional widenings: `drive?: AccessSourceDrive` →
// `drive?: null | AccessSourceDriveDto`, same for `subject`, and
// `caller_role?: GrantRole | null` → `caller_role?: null | RoleDto`.
// `AccessSourceDrive` / `AccessSourceSubject` already alias to their
// Dto siblings (Group A), and `RoleDto` carries the full 5-value
// GrantRole set. Local import so `FolderAncestorsResponse` below
// keeps resolving the short name.
import type { AccessSourceDto as AccessSource } from './generated/openapi/types.gen';
export type { AccessSource };

/**
 * Response envelope of `GET /api/folders/{id}/ancestors`. `ancestors`
 * is root-first, leaf-last (length ≥ 1). `access_source` describes
 * the boundary at element 0 (drive root or share boundary).
 */
export interface FolderAncestorsResponse {
	ancestors: FolderAncestor[];
	access_source: AccessSource;
	/**
	 * Every grant that reaches the leaf — its own, each visible
	 * ancestor's, and the drive's.
	 *
	 * Present ONLY when fetched via `getFolderAncestorsWithGrants()`
	 * (`?include_grants=true`); `undefined` on the breadcrumb's plain
	 * call. Each entry's `resource` names where it sits, so:
	 *
	 *   - `resource.id === leafId`            → direct
	 *   - `resource.type === 'folder'`, other → inherited; resolve the
	 *                                           folder name from `ancestors`
	 *   - `resource.type === 'drive'`         → from the drive
	 */
	effective_grants?: Grant[];
	/**
	 * Public links reaching the leaf, from the same walk. Separate from
	 * `effective_grants` because a link is not a subject — no name to
	 * resolve, no role to change, nobody to notify.
	 *
	 * Same presence rule: only via `getFolderAncestorsWithGrants()`.
	 */
	effective_links?: EffectiveLink[];
}

// Authoritative on the generated SDK — Phase 3 Group D. The handwritten
// shape only diverged in two cosmetic ways: `name?: string` widens to
// `?: string | null`, and the inline `resource: { type, id }` is factored
// out as the generated `ResourceDto` with the same two fields.
// Local import so the sibling `FolderAncestorsResponse` interface above
// keeps resolving the short name.
import type { EffectiveLinkDto as EffectiveLink } from './generated/openapi/types.gen';
export type { EffectiveLink };

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
	| { outcome: 'ok'; count: number; extra?: JobOutcomeExtra }
	| { outcome: 'err'; message: string };

/**
 * The parts of a job outcome's free-form `extra` the panel reads.
 *
 * Deliberately narrow — most keys are per-job counters nothing generic
 * should switch on. These three describe the RUN's shape rather than
 * its work, and the panel has to render them:
 *
 * A run that stopped because the backend was unreachable reports
 * `outcome: 'ok'` — it did not fail, it paused and can be resumed. Read
 * alone that renders as a green "ok" pill, which is exactly wrong: a
 * paused `backend_migration` still holds `migration_readonly` and is
 * refusing writes application-wide. `retryable` is what lets the row
 * say so.
 */
export interface JobOutcomeExtra {
	/** The run stopped at its cursor and can be resumed. */
	paused?: boolean;
	/** It stopped because the ENVIRONMENT failed, not because an
	 *  operator asked — `reason` says what. */
	retryable?: boolean;
	reason?: string;
	[key: string]: unknown;
}

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

/**
 * When a job changes state — `RecoverableJobHandler::mutates()` on the
 * backend. Three values rather than a boolean because the interesting
 * case is conditional: a job can be read-only by default and destructive
 * under `?repair=true`.
 *
 * - `never` — read-only under every flag. Render a read-only badge; no
 *   confirmation needed to trigger.
 * - `always` — changes state on a plain run. Confirm before triggering.
 * - `on_repair_only` — safe to trigger; confirm only when the repair
 *   toggle is on.
 */
export type Mutates = 'never' | 'always' | 'on_repair_only';

/** Wire type of a declared job parameter — `JobParamType` on the backend. */
export type JobParamType = 'boolean' | 'string' | 'number';

/**
 * One run parameter a job accepts, declared by the handler itself
 * (`JobHandler::parameters()`).
 *
 * This is how the panel knows which knobs a job actually reads. It used
 * to guess: `deep` came from a hardcoded name allowlist here, so a job
 * gaining a deep mode needed a frontend release, and a job losing one
 * left a button that silently did nothing. `force` was offered on every
 * job whether or not it was read.
 *
 * `default` is the value the run uses when the parameter is omitted —
 * `null` for a string with no default.
 */
export interface JobParam {
	name: string;
	type: JobParamType;
	default: boolean | number | string | null;
	/** The job's own wording for THIS parameter, for the control's
	 *  tooltip. Absent when the handler left it blank. */
	description?: string;
}

/** Values for one trigger, keyed by declared parameter name. */
export type JobParamValues = Record<string, boolean | number | string>;

export interface JobSummary {
	name: string;
	/** One or two sentences on what the job does, in English, authored
	 *  next to the handler. Absent for jobs that haven't declared one —
	 *  omit the line rather than rendering an empty block. */
	description?: string;
	mutates: Mutates;
	/** Present iff `?repair=true` does something beyond a default run;
	 *  describes what it ADDS. Presence is what gates the repair toggle;
	 *  the text is the confirmation copy. Independent of `mutates` — the
	 *  thumbnail import jobs are `always` AND repair-capable. */
	repair_description?: string;
	/** What this job accepts on a trigger. Absent — not `[]` — when the
	 *  job takes none, so "render no controls" is the natural default. */
	parameters?: JobParam[];
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
	/** Status of this job's most recent run row (recoverable jobs only).
	 *
	 *  **Prefer this over `last_outcome` wherever they could disagree.**
	 *  `last_outcome` is the backend's in-memory record of the last
	 *  dispatch, so anything that changes a run row without running the
	 *  handler leaves it stale — cancelling a Paused run is a direct SQL
	 *  flip, and the panel went on rendering the pause it replaced. */
	last_run_status?: RunStatus;
	/** Findings of the most recent run, counted per severity, read from
	 *  `jobs.run_findings` (recoverable jobs only).
	 *
	 *  **Prefer this over `last_outcome.extra.severity_counts`**, for the
	 *  same reason as `last_run_status`: the outcome is in-memory, so a
	 *  restart erased the counts and a completed run that found data loss
	 *  rendered as a neutral "—". Findings are rows, so this survives.
	 *
	 *  Absent = no run row. `{}` = a run that found nothing — the good
	 *  news, and a different fact. Severity keys are open (the column is
	 *  TEXT), so unknown keys must degrade rather than throw. */
	last_run_severity_counts?: Record<string, number>;
	/** Why the most recent run stopped, read from the run row —
	 *  `backend_unavailable`, `backend_timeout`, `job_failed`,
	 *  `server_restart`.
	 *
	 *  **The outcome pill must not read "ok" while this is set.** A
	 *  retryable pause reports `outcome: "ok"` on the wire — correctly,
	 *  since the run did not fail and a Resume continues it — so taking
	 *  that at face value rendered a green "ok" for a job that gave up
	 *  because its backend was unreachable.
	 *
	 *  Absent means a clean run *or* a pause an operator asked for;
	 *  neither should look alarming, which is why presence is the
	 *  signal rather than `status === "Paused"`. */
	last_run_error_reason?: string;
	/** Present when this job has no cadence of its own but a scheduled
	 *  job runs it — a `*_consistency` detector swept by
	 *  `consistency_batch`. Without it the row renders "on-demand",
	 *  which is false in the default configuration. */
	scheduled_via?: { job: string; interval_ms: number };
	/** Present iff `OXICLOUD_STARTUP_JOBS` names this job — the flags it
	 *  is dispatched with at every boot. Worth showing: a job configured
	 *  with `repair: true` deletes on every restart, and the row would
	 *  otherwise suggest that only happens when someone clicks Run. */
	startup?: StartupTrigger;
}

/**
 * Parameters a job configured in `OXICLOUD_STARTUP_JOBS` runs with,
 * keyed by declared name.
 *
 * A map for the same reason `JobSummary.parameters` is one: the four
 * fixed fields it replaced meant a job growing a parameter silently
 * dropped it from the "at boot" pill.
 */
export interface StartupTrigger {
	params?: JobParamValues;
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
	/** Stable key for why the run stopped — `backend_unavailable`,
	 *  `backend_timeout`, `job_failed`, `server_restart`.
	 *
	 *  Switch on this, never on `error_message`, which is prose and free
	 *  to be reworded. Absent on an operator pause, which is how a
	 *  human-stopped run is told apart from one the environment stopped:
	 *  both are `Paused` in `status`. */
	error_reason?: string;
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

/**
 * Admin sessions-panel row shape. Backend: `SessionSummaryDto` in
 * `src/application/dtos/session_dto.rs`. Deliberately narrower than
 * the DB row — the refresh token is never serialised, and the full
 * DPoP thumbprint is truncated to an 8-char prefix so admins viewing
 * other users' sessions can't exfiltrate the full binding fingerprint.
 */
// Authoritative on the generated SDK — Phase 3 Group D. The three
// nullable fields (`ip_address`, `user_agent`, `dpop_jkt_prefix`)
// widen from `: string | null` to `?: string | null`; `origin`
// moves from an inline 6-string union to the generated `SessionOrigin`
// alias (same 6 values). Local import so `AdminSessionsPage` below
// keeps resolving the short name.
import type { SessionSummaryDto as SessionSummary } from './generated/openapi/types.gen';
export type { SessionSummary };

/** Wire response of `GET /api/admin/sessions`. */
export interface AdminSessionsPage {
	sessions: SessionSummary[];
	limit: number;
	offset: number;
	/** Access-token TTL in seconds — from `OXICLOUD_ACCESS_TOKEN_EXPIRY_SECS`
	 *  server-side. The panel surfaces this in a "revoke takes effect within
	 *  {N} seconds" notice because revoking flips the DB row (breaks refresh)
	 *  but any in-flight JWT stays valid until its `exp`. */
	access_token_expiry_secs: number;
}

// ── /api/config — public server-configuration discovery ────────────────────

// Authoritative on the generated SDK — Phase 3 Group D.
//
// Server-config cluster: `ServerFeatures` / `ServerAuth` / `ServerStatus`
// / `ServerStatusProgress` / `ServerStatusHolder` all become their
// generated siblings. The server-status header pieces (`HolderHeader`
// / `ProgressHeader`) are deliberately HTTP-header-flavoured names on
// the Rust side; aliasing them to the old descriptive names keeps the
// FE vocabulary intact.
//
// Field-level diffs vs the handwritten shapes are all additive /
// nullable widenings — `banners_version?: string` → `?: string | null`,
// `holder?: ServerStatusHolder` → `?: null | HolderHeader`, etc. The
// handwritten `OpaqueMode` string union stays alongside — the generated
// `AuthDto.opaque_mode: string` doesn't tighten it yet (Rust-side
// opportunity for a future enum pass).
import type {
	AuthDto as ServerAuth,
	FeaturesDto as ServerFeatures,
	HeaderPayload as ServerStatus,
	HolderHeader as ServerStatusHolder,
	ProgressHeader as ServerStatusProgress
} from './generated/openapi/types.gen';
export type { ServerAuth, ServerFeatures, ServerStatus, ServerStatusHolder, ServerStatusProgress };

/** One operator-authored banner. Severity drives the visual
 *  variant (warning / notification). `body` holds raw markdown
 *  per locale; the FE picks the viewer's preferred_locale with
 *  `en` as the hard-coded fallback. */
// Authoritative on the generated SDK. The wire-level difference
// vs the pre-SDK hand-written shape:
//   - `severity` becomes the named `OpsBannerSeverity` enum (same
//     'warning' | 'notification' union, re-exported).
//   - `starts_at` / `expires_at` carry `string | null`, matching
//     the server emitting `null` for the "not scheduled / never
//     expires" cases rather than dropping the key.
// Local import so the `ServerStatus.banners` field above can keep
// its typed reference.
import type { OpsBanner, OpsBannerSeverity } from './generated/openapi/types.gen';
export type { OpsBanner, OpsBannerSeverity };

// OPAQUE deployment mode. Kept hand-written for now: the generated
// `AuthDto.opaque_mode` is `string` (the Rust side doesn't carry a
// typed enum yet). SPA code should treat this as the authoritative
// narrowing until the server tightens.
export type OpaqueMode = 'off' | 'migrate' | 'opaque_only';

// `ServerConfig` is the composite — the three sub-DTOs above are
// already aliased, so this one is a pure name-forward.
export type { ServerConfigDto as ServerConfig } from './generated/openapi/types.gen';

// ─── Notifications (Slice E) ─────────────────────────────────────
// Row shape mirrors `application/dtos` output of the Rust backend.
// `payload` stays a raw JSON object (`Record<string, unknown>`) —
// per-kind decoding is a UI concern (kind-specific components read
// what they need from the blob). Adding a new kind server-side does
// NOT churn this file; the FE renders a generic bell row for any
// unknown kind.
// Authoritative on the generated SDK — Phase 3 Group D. The only
// diffs were cosmetic: `payload: Record<string, unknown>` →
// `{ [key: string]: unknown }` and `read_at: string | null` →
// `read_at?: string | null`. Local import so `NotificationListResponse`
// below keeps resolving the short name AND so the SPA's handwritten
// alias wins over the browser's `Notification` global in composable
// scope (a bare `export type { ... as Notification }` doesn't bind
// into module scope and the lib.dom.d.ts global surfaces instead).
import type { NotificationDto as Notification } from './generated/openapi/types.gen';
export type { Notification };

export interface NotificationListResponse {
	items: Notification[];
	unread_count: number;
}

// `GET /api/notifications/unread` + `POST /api/notifications/mark-all-read`
// payload DTOs. The openapi-ts-emitted `UnreadCountResponse` /
// `MarkAllReadResponse` names are status-code wrappers (`{ 200: Dto }
// [keyof …]`), not the body shape — re-exporting the `*Dto` under
// the old names so consumers get the actual payload type.
export type {
	UnreadCountDto as UnreadCountResponse,
	MarkAllReadResponseDto as MarkAllReadResponse
} from './generated/openapi/types.gen';

/** Canonical kind slugs — mirror `domain::entities::notification::kind`. */
export const NOTIFICATION_KIND = {
	SHARE_GRANTED: 'share_granted',
	NEW_LOGIN_FROM_NEW_DEVICE: 'new_login_from_new_device',
	JOB_COMPLETED_FOR_YOU: 'job_completed_for_you',
	STORAGE_QUOTA_THRESHOLD: 'storage_quota_threshold'
} as const;

// `share_granted` notification payload. Authoritative on the
// generated SDK; the hand-written mirror this replaced carried
// `?: string` where the generated version uses `?: string | null`
// — matches what the server actually emits (JSON `null`, not
// dropped keys). See `docs/plan/templated-messages.md § Making
// payloads a real Rust struct` for the original rationale.
export type { SharegrantedPayload } from './generated/openapi/types.gen';
