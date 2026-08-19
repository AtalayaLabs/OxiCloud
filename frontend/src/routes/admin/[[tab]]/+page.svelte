<script lang="ts">
	import { page } from '$app/state';
	import { errorMessage, errorToast } from '$lib/utils/errors';
	import { dateTimeFormatFor } from '$lib/utils/display';
	import {
		clearPluginLogs,
		createUser,
		deletePlugin,
		deleteUser,
		generateEncryptionKey,
		getDashboard,
		getMigration,
		getOidcSettings,
		getPluginLogs,
		getPluginRetention,
		getSmtpInfo,
		getStorageSettings,
		installPlugin,
		listPlugins,
		listUsers,
		listAdminSessions,
		revokeAdminSession,
		getUserAdmin,
		migrationAction,
		reextractAudioMetadata,
		reextractPhotoMetadata,
		promoteUserToInternal,
		resetUserPassword,
		saveOidc,
		savePluginRetention,
		sendSmtpTest,
		setPluginEnabled,
		setRegistrationEnabled,
		setUserActive,
		setUserQuota,
		setUserRole,
		testOidc,
		testStorage,
		rotateStorageEntry,
		createExternalMount,
		deleteExternalMount,
		listExternalMounts,
		type ExternalMount,
		type CreateExternalMountInput,
		type AdminDashboard,
		type GeneratedKey,
		type MigrationStatus,
		type OidcSettings,
		type OidcTestResult,
		type PluginInfo,
		type PluginLogEntry,
		type PluginRetention,
		type ReextractResult,
		addDriveMemberAdmin,
		deleteDriveAdmin,
		listAllDrives,
		listDriveMembersAdmin,
		removeDriveMemberAdmin,
		updateDriveQuota,
		type SmtpInfo,
		type SmtpTestResult,
		type StorageSettings,
		type StorageTestResult
	} from '$lib/api/endpoints/admin';
	import { createDrive, updateDrivePolicies } from '$lib/api/endpoints/drives';
	import {
		ensureResolvers,
		resolveRecipient,
		searchRecipients,
		type Recipient
	} from '$lib/api/endpoints/recipients';
	import type {
		AdminUserSummary,
		Drive,
		DriveMember,
		DrivePolicies,
		DrivePoliciesPartial,
		SessionSummary,
		User
	} from '$lib/api/types';
	import { shortUserAgent } from '$lib/utils/userAgent';
	import { triggerJob } from '$lib/api/endpoints/adminJobs';
	import { serverStatus } from '$lib/stores/serverStatus.svelte';
	import AdminJobsPanel from '$lib/components/AdminJobsPanel.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import OwnerAvatarStack from '$lib/components/OwnerAvatarStack.svelte';
	import PolicyList from '$lib/components/PolicyList.svelte';
	import QuotaEditor from '$lib/components/QuotaEditor.svelte';
	import UserVignette from '$lib/components/UserVignette.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { readPolicyBool } from '$lib/utils/drivePolicies';
	import { session } from '$lib/stores/session.svelte';
	import { drives as drivesStore } from '$lib/stores/drives.svelte';
	import { ui } from '$lib/stores/ui.svelte';
	import { formatBytes } from '$lib/utils/format';

	const PAGE_SIZE = 25;
	const LOGS_PAGE_SIZE = 50;

	/** The signed-in admin's id — used to disable destructive actions on self. */
	const currentAdminId = $derived(session.user?.id ?? '');

	/**
	 * Render an ISO timestamp as a coarse relative time ("3 min ago"). Ported
	 * from formatRelativeTime in static/js/core/formatters.js; an empty/missing
	 * value reads as "Never" (matching the OLD admin user table).
	 */
	function timeAgo(dateStr?: string | null): string {
		if (!dateStr) return t('admin.never', 'Never');
		const then = new Date(dateStr).getTime();
		if (!Number.isFinite(then)) return t('admin.never', 'Never');
		const secs = Math.round((Date.now() - then) / 1000);
		if (secs < 60) return t('admin.time_just_now', 'just now');
		const mins = Math.round(secs / 60);
		if (mins < 60) return t('admin.time_min_ago', { n: mins }, '{{n}} min ago');
		const hours = Math.round(mins / 60);
		if (hours < 24) return t('admin.time_hour_ago', { n: hours }, '{{n}} h ago');
		const days = Math.round(hours / 24);
		if (days < 30) return t('admin.time_day_ago', { n: days }, '{{n}} d ago');
		// `toLocaleDateString()` constructs a fresh Intl.DateTimeFormat per
		// call; the no-options cached formatter is the exact equivalent
		// (both default to numeric year/month/day in the default locale).
		return dateTimeFormatFor(undefined).format(new Date(dateStr));
	}

	/** Quota unit options (bytes per unit) for the quota/create modals. */
	const QUOTA_UNITS = [
		{ value: 1024 ** 2, label: 'MB' },
		{ value: 1024 ** 3, label: 'GB' },
		{ value: 1024 ** 4, label: 'TB' }
	] as const;

	/* ── Styled confirm modal (replaces native confirm) ── */
	let confirmState = $state<{ message: string; resolve: (ok: boolean) => void } | null>(null);
	function showConfirm(message: string): Promise<boolean> {
		return new Promise((resolve) => {
			confirmState = { message, resolve };
		});
	}
	function resolveConfirm(ok: boolean) {
		confirmState?.resolve(ok);
		confirmState = null;
	}

	/* ── User-delete confirm modal ──
	   Destructive-action guard: the admin must re-type the target
	   user's email address to enable the Delete button. Prevents
	   fat-finger deletion — a single accidental click on the wrong
	   row won't wipe an account. The admin still bears final
	   responsibility; this is UX friction, not authorization. */
	let deleteUserModal = $state<{ userId: string; username: string; email: string } | null>(null);
	let deleteUserEmailInput = $state('');
	let deleteUserBusy = $state(false);
	const deleteUserEmailMatches = $derived(
		deleteUserModal !== null &&
			deleteUserEmailInput.trim().toLowerCase() === deleteUserModal.email.toLowerCase()
	);
	function openDeleteUser(u: AdminUserSummary) {
		deleteUserModal = {
			userId: u.id,
			username: u.username || u.email,
			email: u.email
		};
		deleteUserEmailInput = '';
	}
	async function confirmDeleteUser() {
		if (!deleteUserModal || !deleteUserEmailMatches) return;
		deleteUserBusy = true;
		try {
			await deleteUser(deleteUserModal.userId);
			deleteUserModal = null;
			await loadUsers();
		} catch (e) {
			reportError(e);
		} finally {
			deleteUserBusy = false;
		}
	}

	type Tab =
		| 'dashboard'
		| 'users'
		| 'sessions'
		| 'drives'
		| 'mounts'
		| 'plugins'
		| 'oidc'
		| 'storage'
		| 'smtp'
		| 'jobs';

	const VALID_TABS: readonly Tab[] = [
		'dashboard',
		'users',
		'sessions',
		'drives',
		'mounts',
		'plugins',
		'oidc',
		'storage',
		'smtp',
		'jobs'
	];

	function parseTab(raw: string | undefined): Tab {
		return VALID_TABS.includes(raw as Tab) ? (raw as Tab) : 'dashboard';
	}

	// URL is the source of truth for the current tab. Navigation
	// is via the AppShell sidebar (see `ADMIN_LINKS` there);
	// this page just reads `page.params.tab` and renders the
	// matching content block. Unidirectional URL→state means no
	// `$effect` loop is even possible.
	const tab = $derived<Tab>(parseTab(page.params.tab));

	/**
	 * Human-readable label for the current section — feeds the
	 * page title (`Admin › Jobs · OxiCloud`) and the h1. Kept in
	 * sync with the `ADMIN_LINKS` labels in AppShell manually
	 * (small list, unlikely to drift). Extracting to a shared
	 * module would be over-engineering for 9 strings.
	 */
	const tabLabel = $derived.by<string>(() => {
		switch (tab) {
			case 'dashboard':
				return t('admin.dashboard', 'Dashboard');
			case 'users':
				return t('admin.users', 'Users');
			case 'sessions':
				return t('admin.sessions', 'Sessions');
			case 'drives':
				return t('admin.drives', 'Drives');
			case 'mounts':
				return t('admin.mounts', 'External Mounts');
			case 'plugins':
				return t('admin.plugins', 'Plugins');
			case 'oidc':
				return t('admin.oidc', 'OIDC / SSO');
			case 'storage':
				return t('admin.storage_tab', 'Storage');
			case 'smtp':
				return t('admin.smtp', 'Email (SMTP)');
			case 'jobs':
				return t('admin.jobs.tab', 'Background tasks');
		}
	});

	// Dashboard
	let dashboard = $state<AdminDashboard | null>(null);
	let dashboardError = $state<string | null>(null);

	// External mounts
	let mounts = $state<ExternalMount[] | null>(null);
	let mountsError = $state<string | null>(null);
	let newMount = $state<CreateExternalMountInput>({ name: '', host_path: '', read_only: false });
	let mountCreating = $state(false);

	async function loadMounts() {
		mountsError = null;
		try {
			mounts = await listExternalMounts();
		} catch (e) {
			mountsError = errorMessage(e);
		}
	}

	async function createMount() {
		if (!newMount.name.trim() || !newMount.host_path.trim()) return;
		mountCreating = true;
		try {
			const created = await createExternalMount(newMount);
			mounts = [...(mounts ?? []), created];
			newMount = { name: '', host_path: '', read_only: false };
		} catch (e) {
			mountsError = errorMessage(e);
		} finally {
			mountCreating = false;
		}
	}

	async function deleteMount(id: string) {
		if (!(await showConfirm('Remove this mount? Files on the host are kept.'))) return;
		try {
			await deleteExternalMount(id);
			mounts = mounts?.filter((m) => m.mount_folder_id !== id) ?? null;
		} catch (e) {
			reportError(e);
			await loadMounts();
		}
	}

	// SMTP
	let smtp = $state<SmtpInfo | null>(null);
	let smtpTo = $state('');
	let smtpResult = $state<SmtpTestResult | null>(null);
	let smtpSending = $state(false);

	async function loadDashboard() {
		dashboardError = null;
		try {
			dashboard = await getDashboard();
		} catch (e) {
			dashboardError = errorMessage(e);
		}
	}

	async function toggleRegistration(enabled: boolean) {
		try {
			await setRegistrationEnabled(enabled);
			if (dashboard) dashboard.registration_enabled = enabled;
		} catch (e) {
			reportError(e);
			await loadDashboard();
		}
	}

	async function loadSmtp() {
		try {
			smtp = await getSmtpInfo();
		} catch (e) {
			reportError(e);
		}
	}

	async function runSmtpTest() {
		if (!smtpTo.trim()) return;
		smtpSending = true;
		smtpResult = null;
		try {
			smtpResult = await sendSmtpTest(smtpTo.trim());
		} catch (e) {
			smtpResult = { success: false, message: errorMessage(e) };
		} finally {
			smtpSending = false;
		}
	}

	// OIDC
	let oidc = $state<(OidcSettings & { client_secret?: string }) | null>(null);
	let oidcTest = $state<OidcTestResult | null>(null);
	let oidcMsg = $state<{ text: string; ok: boolean } | null>(null);
	let oidcSaving = $state(false);

	async function loadOidc() {
		try {
			oidc = await getOidcSettings();
		} catch (e) {
			oidcMsg = { text: errorMessage(e), ok: false };
		}
	}
	async function runOidcTest() {
		if (!oidc?.issuer_url) return;
		oidcTest = await testOidc(oidc.issuer_url);
		if (oidcTest.success && oidcTest.provider_name_suggestion && !oidc.provider_name) {
			oidc.provider_name = oidcTest.provider_name_suggestion;
		}
	}
	async function doSaveOidc() {
		if (!oidc) return;
		oidcSaving = true;
		oidcMsg = null;
		try {
			await saveOidc({
				enabled: oidc.enabled,
				issuer_url: oidc.issuer_url.trim(),
				client_id: oidc.client_id.trim(),
				client_secret: oidc.client_secret || null,
				scopes: oidc.scopes || null,
				auto_provision: oidc.auto_provision,
				admin_groups: oidc.admin_groups || null,
				disable_password_login: oidc.disable_password_login,
				provider_name: oidc.provider_name || null
			});
			oidcMsg = { text: t('admin.settings_saved_ok', 'Settings saved.'), ok: true };
		} catch (e) {
			oidcMsg = { text: errorMessage(e), ok: false };
		} finally {
			oidcSaving = false;
		}
	}

	// Storage — multi-entry read-only view.
	//
	// Post `docs/plan/storage-multi-entry.md`, the .env is the SOLE
	// place to declare backends. The admin storage tab is now:
	//   - a read-only list of the entries the server booted with,
	//   - a per-entry test button (round-trip against that entry),
	//   - a per-entry audit action (triggers blobs_consistency?storage=<name>),
	//   - a per-non-active migrate+activate button,
	//   - the migration status line + cutover hint.
	// No form. No save. The retired save endpoint / DTO are still on
	// the backend during the deprecation window but the UI never
	// hits them.
	let storage = $state<StorageSettings | null>(null);
	let storageMsg = $state<{ text: string; ok: boolean } | null>(null);
	// Per-entry test state — keyed by entry name so the buttons don't
	// step on each other and the last result stays visible per row.
	let entryTest = $state<
		Record<string, { busy: boolean; result?: StorageTestResult; error?: string }>
	>({});

	async function loadStorage() {
		try {
			storage = await getStorageSettings();
		} catch (e) {
			storageMsg = { text: errorMessage(e), ok: false };
		}
	}

	async function doTestEntry(name: string) {
		entryTest = { ...entryTest, [name]: { busy: true } };
		try {
			const r: StorageTestResult = await testStorage({ entry_name: name });
			entryTest = { ...entryTest, [name]: { busy: false, result: r } };
		} catch (e) {
			entryTest = { ...entryTest, [name]: { busy: false, error: errorMessage(e) } };
		}
	}

	// Entry-card action confirmations use `ui.notify()` (viewport
	// toast) rather than `storageMsg` (top-of-tab banner). The
	// buttons live on cards that can be scrolled far below the
	// storage-msg region — a banner confirmation is invisible
	// when the user is looking at the card that triggered it.
	async function doAuditEntry(name: string) {
		try {
			await triggerJob('blobs_consistency', { storage: name });
			ui.notify(
				t(
					'admin.storage_audit_triggered',
					{ name },
					'blobs_consistency triggered for `{{name}}` — watch it on the Jobs tab.'
				),
				'success'
			);
		} catch (e) {
			ui.notify(errorMessage(e), 'error');
		}
	}

	// K4: `backend_consistency` — the mirror of `blobs_consistency`.
	// Walks the entry's backend and reports blobs physically present
	// on it that have no matching row in `storage.blobs` (orphans on
	// disk/S3). Meaningful for ANY entry, not just the active one —
	// useful for spotting leftover data on a deprecated backend.
	async function doStorageConsistency(name: string) {
		try {
			await triggerJob('backend_consistency', { storage: name });
			ui.notify(
				t(
					'admin.storage_backend_audit_triggered',
					{ name },
					'backend_consistency triggered for `{{name}}` — watch it on the Background tasks tab.'
				),
				'success'
			);
		} catch (e) {
			ui.notify(errorMessage(e), 'error');
		}
	}

	async function doMigrateActivate(name: string) {
		if (
			!confirm(
				t(
					'admin.storage_migrate_confirm',
					{ name },
					'Migrate all blobs to `{{name}}` and set it as the active entry? The server enters read-only mode during the copy; the live backend swaps automatically on completion (no restart needed).'
				)
			)
		)
			return;
		await doMigration('start', name);
	}

	// K4 storage-key-rotation: normalise every blob on `<name>` to
	// that entry's head-pair format. Unlike migration, rotation does
	// NOT engage read-only mode — uploads/reads keep working
	// throughout. Fire-and-forget; the Jobs tab surfaces progress.
	async function doRotateEntry(name: string) {
		if (
			!confirm(
				t(
					'admin.backend_rotate_confirm',
					{ name },
					'Start a background rotation on `{{name}}`? Every existing blob is rewritten under the entry’s head pair (v1 header + head key). All operations continue normally during rotation — no read-only mode. Progress shows in the top banner and on the Jobs tab.'
				)
			)
		)
			return;
		try {
			await rotateStorageEntry(name);
			ui.notify(
				t(
					'admin.backend_rotate_triggered',
					{ name },
					'Rotation started on `{{name}}` — watch it on the Jobs tab (`backend_rotate`).'
				),
				'success'
			);
		} catch (e) {
			ui.notify(errorMessage(e), 'error');
		}
	}

	// Migration
	let migration = $state<MigrationStatus | null>(null);
	let migrationTimer: ReturnType<typeof setInterval> | null = null;

	function stopMigrationPoll() {
		if (migrationTimer) {
			clearInterval(migrationTimer);
			migrationTimer = null;
		}
	}
	// Polling model for the migration flow:
	//   1. `lastMigrationStatus` — last observed status. Storage
	//      state is refreshed on any transition so the readonly
	//      banner + active-entry indicators track the server without
	//      the admin having to reload the tab.
	//   2. `sawActive` — flips true the first time we observe
	//      `running` or `paused` after a user action. We only STOP
	//      polling on `idle` / `completed` / `failed` AFTER
	//      `sawActive` is true — otherwise a trigger endpoint's
	//      instant-202 response (the run row hasn't landed in the
	//      DB yet) would kill the poll loop before the migration
	//      even started, and the banner + active-entry update
	//      would never show up until the admin manually refreshed.
	//   3. `pendingSince` — timestamp of the last user action.
	//      Bounds how long we keep polling on `idle` while waiting
	//      for the run row to appear. If the run never opens within
	//      the grace window (60 s — dispatch spawn + DB insert
	//      normally takes < 100 ms), we give up.
	let lastMigrationStatus: string | undefined;
	let sawActive = false;
	let pendingSince: number | undefined;
	const PENDING_GRACE_MS = 60_000;
	async function loadMigration() {
		try {
			migration = await getMigration();
			const status = migration.status;
			const active = status === 'running' || status === 'paused';
			if (active) sawActive = true;

			// Refresh storage on any status change so the readonly
			// banner + active-entry indicators reflect current
			// server state.
			if (status !== lastMigrationStatus) {
				lastMigrationStatus = status;
				void loadStorage();
			}

			// Keep polling while the migration is active OR while
			// we're within the grace window waiting for a
			// user-triggered run to appear.
			const withinGrace =
				!sawActive && pendingSince != null && performance.now() - pendingSince < PENDING_GRACE_MS;
			if (active || withinGrace) {
				if (!migrationTimer) migrationTimer = setInterval(loadMigration, 5000);
			} else {
				// Not active and (either we've already seen it run OR
				// the grace window ran out) → stop polling.
				stopMigrationPoll();
				pendingSince = undefined;
			}
		} catch {
			stopMigrationPoll();
			pendingSince = undefined;
		}
	}
	async function doMigration(action: 'start' | 'pause' | 'resume', targetName?: string) {
		try {
			await migrationAction(action, targetName);
			// Reset the status memo so the very next `loadMigration`
			// tick unconditionally reloads storage (an admin-triggered
			// action is exactly when the readonly flag flips).
			lastMigrationStatus = undefined;
			sawActive = false;
			pendingSince = performance.now();
			await loadMigration();
		} catch (e) {
			reportError(e);
		}
	}

	// The old target-name picker state was retired — the entries
	// table now has per-row "Migrate & activate" buttons on
	// non-active entries. Simpler mental model; no picker to sync.

	// Retired: the .env cutover-hint state (cutoverPending +
	// cutoverEnvLines + cutoverCopied + copyCutoverEnv). It served
	// the pre-multi-entry flow that made admins paste env vars
	// into .env after migration. Post-multi-entry, the server
	// writes `active_backend_name` to the DB automatically on
	// migration completion; the operator just restarts. The new
	// short "restart to switch" hint is rendered inline in the
	// entries card template, no derived state needed.

	// Migration integrity verification retired in slice 7 — the
	// sample-based /storage/migration/verify endpoint is replaced by
	// `POST /api/admin/jobs/blobs_consistency/trigger?storage=<name>`,
	// a full walk. Operators trigger it from the Jobs tab.

	const migrationPct = $derived(
		migration && migration.total_blobs > 0
			? Math.round((migration.migrated_blobs / migration.total_blobs) * 100)
			: 0
	);
	/** Estimated minutes remaining, derived from throughput + average blob size. */
	const migrationEtaMin = $derived.by(() => {
		const m = migration;
		if (!m || m.status !== 'running' || !m.throughput_bytes_per_sec) return null;
		const remaining = m.total_blobs - m.migrated_blobs;
		if (remaining <= 0 || m.migrated_blobs <= 0) return null;
		const avgBlobSize = m.migrated_bytes / m.migrated_blobs;
		const etaSecs = (remaining * avgBlobSize) / m.throughput_bytes_per_sec;
		return Math.ceil(etaSecs / 60);
	});

	// Plugin logs
	let logsPlugin = $state<PluginInfo | null>(null);
	let logs = $state<PluginLogEntry[]>([]);
	let logsLevel = $state('');
	let logsSearch = $state('');
	let logsLoading = $state(false);
	let logsPage = $state(0);
	let logsTotal = $state(0);
	let logsLive = $state(true);
	let logStream: EventSource | null = null;

	/** Best-effort message text across the persisted (`msg`) and legacy shapes. */
	function logMsg(e: PluginLogEntry): string {
		return e.msg ?? e.message ?? '';
	}
	/** Kind column: outcome entries surface their reason, others read "log". */
	function logKind(e: PluginLogEntry): string {
		return e.kind === 'outcome' ? (e.reason ?? 'outcome') : 'log';
	}

	function stopLogStream() {
		if (logStream) {
			logStream.close();
			logStream = null;
		}
	}

	/** Open the SSE live tail for the current plugin (no-op when Live is off). */
	function startLogStream() {
		stopLogStream();
		if (!logsPlugin || !logsLive) return;
		const es = new EventSource(
			`/api/admin/plugins/${encodeURIComponent(logsPlugin.id)}/logs/stream`,
			{ withCredentials: true }
		);
		es.onmessage = (ev) => {
			try {
				onLiveLogEntry(JSON.parse(ev.data) as PluginLogEntry);
			} catch {
				/* ignore malformed frames */
			}
		};
		// Fell behind the broadcast buffer — resync from the server.
		es.addEventListener('lagged', () => void loadLogs());
		logStream = es;
	}

	/**
	 * Prepend a streamed entry, but only on the newest page and when it passes
	 * the active filter — so the live tail never fights pagination.
	 */
	function onLiveLogEntry(entry: PluginLogEntry) {
		if (logsPage !== 0) return;
		if (logsLevel && (entry.level ?? '').toLowerCase() !== logsLevel.toLowerCase()) return;
		if (logsSearch && !logMsg(entry).toLowerCase().includes(logsSearch.toLowerCase())) return;
		logs = [entry, ...logs].slice(0, LOGS_PAGE_SIZE);
		logsTotal += 1;
	}

	function toggleLive() {
		if (logsLive) startLogStream();
		else stopLogStream();
	}

	function logsPrev() {
		if (logsPage > 0) {
			logsPage--;
			void loadLogs();
		}
	}
	function logsNext() {
		if ((logsPage + 1) * LOGS_PAGE_SIZE < logsTotal) {
			logsPage++;
			void loadLogs();
		}
	}

	// Plugin detail (metadata + retention) — opened alongside logs
	let retention = $state<PluginRetention | null>(null);
	let retentionDays = $state(0);
	let retentionMb = $state(0);
	let retentionMsg = $state<string | null>(null);

	async function openLogs(p: PluginInfo) {
		logsPlugin = p;
		retention = null;
		retentionMsg = null;
		logsPage = 0;
		logsLevel = '';
		logsSearch = '';
		await Promise.all([loadLogs(), loadRetention(p.id)]);
		startLogStream();
	}

	function closeLogs() {
		stopLogStream();
		logsPlugin = null;
		logs = [];
		logsTotal = 0;
		logsPage = 0;
	}

	/** Reset to the first page (filter changed) then reload. */
	function reloadLogsFromStart() {
		logsPage = 0;
		void loadLogs();
	}
	async function loadRetention(id: string) {
		try {
			retention = await getPluginRetention(id);
			if (retention) {
				retentionDays = retention.retention_days;
				retentionMb = Math.round(retention.max_bytes / (1024 * 1024));
			}
		} catch {
			/* retention is optional — leave unset on error */
		}
	}
	async function saveRetention() {
		if (!logsPlugin) return;
		retentionMsg = null;
		if (
			!Number.isFinite(retentionDays) ||
			retentionDays < 0 ||
			!Number.isFinite(retentionMb) ||
			retentionMb < 0
		) {
			retentionMsg = t('admin.plugins_retention_invalid', 'Enter non-negative numbers.');
			return;
		}
		try {
			await savePluginRetention(logsPlugin.id, {
				retention_days: Math.round(retentionDays),
				max_bytes: Math.round(retentionMb) * 1024 * 1024
			});
			retentionMsg = t('admin.plugins_retention_saved', 'Retention saved.');
		} catch (e) {
			retentionMsg = errorMessage(e);
		}
	}
	async function purgeLogs() {
		if (!logsPlugin) return;
		if (
			!(await showConfirm(t('admin.plugins_logs_confirm_clear', 'Clear all logs for this plugin?')))
		)
			return;
		try {
			await clearPluginLogs(logsPlugin.id);
			logsPage = 0;
			await loadLogs();
		} catch (e) {
			reportError(e);
		}
	}

	// Plugin install (.zip upload)
	let installing = $state(false);
	let installMsg = $state<{ ok: boolean; text: string } | null>(null);

	async function onInstallPlugin(e: Event) {
		const input = e.currentTarget as HTMLInputElement;
		const file = input.files?.[0];
		if (!file) return;
		installing = true;
		installMsg = null;
		try {
			const info = await installPlugin(file);
			installMsg = {
				ok: true,
				text: t('admin.plugins_installed', { name: info.name }, `Installed ${info.name}.`)
			};
			await loadPlugins();
		} catch (err) {
			installMsg = { ok: false, text: errorMessage(err) };
		} finally {
			installing = false;
			input.value = '';
		}
	}
	async function loadLogs() {
		if (!logsPlugin) return;
		logsLoading = true;
		try {
			const page = await getPluginLogs(logsPlugin.id, {
				level: logsLevel,
				search: logsSearch,
				limit: LOGS_PAGE_SIZE,
				offset: logsPage * LOGS_PAGE_SIZE
			});
			logs = page.entries;
			logsTotal = page.total;
		} catch (e) {
			reportError(e);
		} finally {
			logsLoading = false;
		}
	}

	// Users
	let users = $state<AdminUserSummary[]>([]);
	let total = $state(0);
	let pageIndex = $state(0);
	let usersError = $state<string | null>(null);
	let createOpen = $state(false);
	let createError = $state<string | null>(null);
	let creating = $state(false);
	let newUser = $state({
		username: '',
		email: '',
		password: '',
		role: 'user',
		quotaValue: 5,
		quotaUnit: (1024 ** 3) as number
	});

	// User-envelope quota edit modal state. Draft (unlimited flag,
	// value, unit) lives inside <QuotaEditor>; the parent only tracks
	// the subject (which user) and the current bytes to seed with.
	let quotaModal = $state<{
		userId: string;
		username: string;
		initialBytes: number;
	} | null>(null);
	let quotaModalBusy = $state(false);
	let quotaModalError = $state<string | null>(null);

	// Reset-password modal
	let resetModal = $state<{ userId: string; username: string } | null>(null);
	let resetPassword = $state('');
	let resetError = $state<string | null>(null);
	let resetting = $state(false);

	// Sessions (admin panel — see task #52 / docs/plan/dpop.md Gate 10).
	// Global cross-user listing by default; user-filter dropdown narrows.
	// Active-only by default (hides revoked + expired); checkbox opts into
	// showing everything for forensics. Revoke action mutates in place —
	// the row is refetched to update `is_revoked` badge.
	let sessions = $state<SessionSummary[]>([]);
	let sessionsError = $state<string | null>(null);
	let sessionsLoading = $state(false);
	let sessionsFilterUserId = $state<string>('');
	let sessionsIncludeRevoked = $state(false);
	let sessionRevokingId = $state<string | null>(null);
	// Access-token TTL served alongside the sessions page — drives the
	// "revoke takes effect within {N} seconds" warning. Revoke flips the
	// DB row (breaks the refresh path), but a JWT already in flight
	// stays valid until its `exp`. Populated on the first load, reused
	// for every render — the value is server-config, not per-request.
	let sessionsAccessTokenExpirySecs = $state<number | null>(null);

	async function loadSessions() {
		sessionsLoading = true;
		sessionsError = null;
		try {
			const page = await listAdminSessions({
				userId: sessionsFilterUserId || undefined,
				includeRevoked: sessionsIncludeRevoked,
				limit: PAGE_SIZE
			});
			sessions = page.sessions;
			sessionsAccessTokenExpirySecs = page.access_token_expiry_secs;
		} catch (e) {
			sessionsError = errorMessage(e);
		} finally {
			sessionsLoading = false;
		}
	}

	async function onRevokeSession(id: string, isCurrent: boolean) {
		// Escalated warning for the caller's own session — revoking it
		// bricks the tab (all subsequent requests 401 → nav-guard bounces
		// to /login). A plain "are you sure" was too easy to click
		// through by muscle memory on a table of revoke buttons.
		const message = isCurrent
			? t(
					'admin.sessions.revoke_self_confirm',
					"⚠️  This is YOUR current session. Revoking it will log YOU out immediately and you'll have to sign back in. Continue?"
				)
			: t(
					'admin.sessions.revoke_confirm',
					'Revoke this session? The next request from that browser will 401.'
				);
		if (!confirm(message)) return;
		sessionRevokingId = id;
		try {
			await revokeAdminSession(id);
			await loadSessions();
		} catch (e) {
			sessionsError = errorMessage(e);
		} finally {
			sessionRevokingId = null;
		}
	}

	// Plugins
	let plugins = $state<PluginInfo[]>([]);
	let pluginsAvailable = $state(true);
	let pluginsError = $state<string | null>(null);

	async function loadUsers() {
		usersError = null;
		try {
			const page = await listUsers(PAGE_SIZE, pageIndex * PAGE_SIZE);
			users = page.users;
			total = page.total;
		} catch (e) {
			usersError = errorMessage(e);
		}
	}

	async function loadPlugins() {
		pluginsError = null;
		try {
			const res = await listPlugins();
			pluginsAvailable = res.available;
			plugins = res.plugins;
		} catch (e) {
			pluginsError = errorMessage(e);
		}
	}

	function reportError(e: unknown) {
		errorToast(e);
	}

	// ── Maintenance: bulk metadata re-extraction ─────────────────────────────
	let audioBusy = $state(false);
	let audioResult = $state<ReextractResult | null>(null);
	let photoBusy = $state(false);
	let photoResult = $state<ReextractResult | null>(null);

	async function runAudioReindex() {
		audioBusy = true;
		audioResult = null;
		try {
			audioResult = await reextractAudioMetadata();
		} catch (e) {
			reportError(e);
		} finally {
			audioBusy = false;
		}
	}

	async function runPhotoReindex() {
		photoBusy = true;
		photoResult = null;
		try {
			photoResult = await reextractPhotoMetadata();
		} catch (e) {
			reportError(e);
		} finally {
			photoBusy = false;
		}
	}

	// ── Storage: generate an at-rest encryption key ──────────────────────────
	let keyBusy = $state(false);
	let generatedKey = $state<GeneratedKey | null>(null);

	async function runGenerateKey() {
		keyBusy = true;
		try {
			generatedKey = await generateEncryptionKey();
		} catch (e) {
			reportError(e);
		} finally {
			keyBusy = false;
		}
	}

	async function copyText(text: string) {
		try {
			await navigator.clipboard.writeText(text);
			ui.notify(t('common.copied', 'Copied to clipboard'), 'success');
		} catch {
			ui.notify(t('common.copy_failed', 'Copy failed'), 'error');
		}
	}

	/** True when a settings field is locked by an OXICLOUD_* env var. */
	function isEnvLocked(overrides: string[] | undefined, field: string): boolean {
		return Array.isArray(overrides) && overrides.includes(field);
	}

	/** True for the signed-in admin's own row — guards self-destructive actions. */
	function isSelf(u: AdminUserSummary): boolean {
		return u.id === currentAdminId;
	}
	/** OIDC/SSO-provisioned account (no local password to reset). */
	function isOidcUser(u: AdminUserSummary): boolean {
		return u.federation_kind === 'oidc';
	}
	/** Used-quota percentage (0 when unlimited) for the per-user progress bar. */
	function quotaPct(u: AdminUserSummary): number {
		return u.storage_quota_bytes > 0 ? (u.storage_used_bytes / u.storage_quota_bytes) * 100 : 0;
	}

	async function toggleRole(u: AdminUserSummary) {
		if (isSelf(u)) return;
		const role = u.role === 'admin' ? 'user' : 'admin';
		if (!(await showConfirm(t('admin.confirm_role', { role }, 'Change role to {{role}}?')))) return;
		try {
			await setUserRole(u.id, role);
			await loadUsers();
		} catch (e) {
			reportError(e);
		}
	}

	async function toggleActive(u: AdminUserSummary) {
		if (isSelf(u) && u.active) return;
		const msg = u.active
			? t('admin.confirm_deactivate', 'Deactivate this user?')
			: t('admin.confirm_activate', 'Activate this user?');
		if (!(await showConfirm(msg))) return;
		try {
			await setUserActive(u.id, !u.active);
			await loadUsers();
		} catch (e) {
			reportError(e);
		}
	}

	function openQuota(u: AdminUserSummary) {
		quotaModalError = null;
		quotaModal = {
			userId: u.id,
			username: u.username || u.email,
			initialBytes: u.storage_quota_bytes
		};
	}
	// `unlimited` maps to 0 on the wire — that's the backend
	// convention for the user envelope (`quota <= 0` short-circuit
	// in `eval_user_envelope`). Encoding the "unlimited" concept
	// happens here, so the shared <QuotaEditor> doesn't have to
	// know about endpoint-specific magic values.
	async function saveQuota(result: { unlimited: boolean; bytes: number }) {
		if (!quotaModal) return;
		quotaModalBusy = true;
		quotaModalError = null;
		try {
			await setUserQuota(quotaModal.userId, result.unlimited ? 0 : result.bytes);
			quotaModal = null;
			await loadUsers();
		} catch (e) {
			quotaModalError = errorMessage(e);
		} finally {
			quotaModalBusy = false;
		}
	}

	function openReset(u: AdminUserSummary) {
		resetModal = { userId: u.id, username: u.username || u.email };
		resetPassword = '';
		resetError = null;
	}
	async function submitReset(e: SubmitEvent) {
		e.preventDefault();
		if (!resetModal) return;
		if (resetPassword.length < 8) {
			resetError = t('admin.error_password_short', 'Password must be at least 8 characters.');
			return;
		}
		resetting = true;
		resetError = null;
		try {
			await resetUserPassword(resetModal.userId, resetPassword);
			resetModal = null;
			ui.notify(t('admin.password_reset', 'Password reset'), 'success');
		} catch (err) {
			resetError = errorMessage(err);
		} finally {
			resetting = false;
		}
	}

	function removeUser(u: AdminUserSummary) {
		if (isSelf(u)) return;
		openDeleteUser(u);
	}

	// External → internal promotion. Confirms first because the mutation
	// provisions a home drive + flips the is_external flag; irreversible
	// via the admin UI (there's no demote endpoint on purpose). Backend
	// refuses when magic-link login is disabled — surfaced as a toast.
	async function promoteExternal(u: AdminUserSummary) {
		if (!u.is_external) return;
		if (
			!(await showConfirm(
				t(
					'admin.confirm_promote_user',
					{ name: u.username || u.email },
					'Promote {{name}} to an internal user? This provisions a home drive and gives the account a normal storage envelope. The account keeps its identity; magic-link login stays the way in unless a password is set later.'
				)
			))
		)
			return;
		try {
			await promoteUserToInternal(u.id);
			await loadUsers();
		} catch (e) {
			reportError(e);
		}
	}

	async function submitCreate(e: SubmitEvent) {
		e.preventDefault();
		const username = newUser.username.trim();
		const email = newUser.email.trim();
		if (username.length < 3) {
			createError = t('admin.error_username_short', 'Username must be at least 3 characters.');
			return;
		}
		if (newUser.password.length < 8) {
			createError = t('admin.error_password_short', 'Password must be at least 8 characters.');
			return;
		}
		creating = true;
		createError = null;
		try {
			await createUser({
				username,
				// Email is optional — the backend auto-generates one when blank.
				email: email || null,
				password: newUser.password,
				role: newUser.role,
				quota_bytes: Math.round(newUser.quotaValue * newUser.quotaUnit)
			});
			createOpen = false;
			newUser = {
				username: '',
				email: '',
				password: '',
				role: 'user',
				quotaValue: 5,
				quotaUnit: 1024 ** 3
			};
			await loadUsers();
		} catch (err) {
			createError = errorMessage(err);
		} finally {
			creating = false;
		}
	}

	// ── Drives (D3a admin create-shared-drive) ───────────────────────────────
	let drivesList = $state<Drive[]>([]);
	let drivesError = $state<string | null>(null);
	let driveCreateOpen = $state(false);
	let driveCreating = $state(false);
	let driveCreateError = $state<string | null>(null);
	let driveForm = $state({
		name: '',
		ownerQuery: '',
		ownerPick: null as Recipient | null,
		quotaValue: 0,
		quotaUnit: (1024 ** 3) as number
	});
	let ownerSuggestions = $state<Recipient[]>([]);
	let ownerSearching = $state(false);
	let ownerSearchToken = 0;

	// Members keyed by drive id. The admin Drives table renders an Owner
	// avatar stack per row; we lazily fetch members for each drive in
	// parallel after the drives listing comes back. Missing entries mean
	// "still loading" — the stack treats undefined as no-owners-yet.
	let driveMembers = $state<Record<string, DriveMember[]>>({});

	// Owner user record for each PERSONAL drive, keyed by drive id.
	// Personal drives have exactly one user-subject owner and their
	// `d.quota_bytes` is null — the effective quota comes from the
	// owner user's `storage_quota_bytes` envelope (see memory
	// `project_user_envelope_quota_model`). We resolve the owner
	// once at load time via the admin-scoped `/api/admin/users/{id}`
	// endpoint so the Name column can show the username instead of
	// the drive UUID, and the Quota column can display the envelope
	// cap. Shared drives are absent from this map.
	let personalDriveOwners = $state<Record<string, User>>({});

	async function loadDrivesTab() {
		drivesError = null;
		try {
			// `/api/admin/drives` — system-wide view; an admin who creates
			// a drive for another user has no `role_grants` row on it and
			// wouldn't see it via the user-facing `/api/drives` listing.
			drivesList = await listAllDrives();
		} catch (e) {
			drivesError = errorMessage(e);
			return;
		}
		// Seed the contact + group caches so the avatar stack renders real
		// labels (and stable initials/colours) instead of bare UUIDs.
		void ensureResolvers();
		// Fan out one members fetch per drive in parallel. A swallowed
		// error per drive degrades gracefully — that row's stack shows
		// "No owners" rather than blocking the whole page.
		const nextMembers: Record<string, DriveMember[]> = {};
		await Promise.all(
			drivesList.map(async (d) => {
				try {
					nextMembers[d.id] = await listDriveMembersAdmin(d.id);
				} catch {
					nextMembers[d.id] = [];
				}
			})
		);
		driveMembers = nextMembers;

		// Resolve owner users for personal drives — used by the Name
		// column (username instead of UUID) and the Quota column
		// (envelope cap instead of "∞" for a NULL drive.quota_bytes).
		// getUserAdmin caches per-id at module scope, so N personal
		// drives owned by the same user share one fetch. Runs after
		// `driveMembers` is populated because the owner subject id
		// comes from the first user-typed member of the drive.
		const nextOwners: Record<string, User> = {};
		await Promise.all(
			drivesList
				.filter((d) => d.kind === 'personal')
				.map(async (d) => {
					const ownerMember = nextMembers[d.id]?.find((m) => m.subject.type === 'user');
					if (!ownerMember) return;
					const user = await getUserAdmin(ownerMember.subject.id);
					if (user) nextOwners[d.id] = user;
				})
		);
		personalDriveOwners = nextOwners;
	}

	function driveKindLabel(d: Drive): string {
		if (d.kind === 'shared') return t('admin.drive_kind_shared', 'Shared');
		return t('admin.drive_kind_personal', 'Personal');
	}
	// The "(default)" annotation renders on a separate line under the
	// kind badge so the Kind column stays narrow (users' request).
	function driveDefaultSuffix(d: Drive): string | null {
		return d.default_for_user ? t('admin.drive_kind_default_suffix', '(default)') : null;
	}

	function openDriveCreate() {
		driveForm = { name: '', ownerQuery: '', ownerPick: null, quotaValue: 0, quotaUnit: 1024 ** 3 };
		ownerSuggestions = [];
		driveCreateError = null;
		driveCreateOpen = true;
	}

	// Search runs in the background; a monotonically-incrementing `token`
	// guards against out-of-order results overwriting a newer query — the
	// network races by query length and keystroke timing.
	async function searchOwnerCandidates(q: string) {
		driveForm.ownerPick = null;
		const trimmed = q.trim();
		if (!trimmed) {
			ownerSuggestions = [];
			return;
		}
		const token = ++ownerSearchToken;
		ownerSearching = true;
		try {
			// `includeSelf` — admin creating a drive may legitimately want to
			// own it themselves; the default share-modal "no self" rule
			// doesn't apply in the admin context.
			const results = await searchRecipients(trimmed, { includeSelf: true });
			if (token !== ownerSearchToken) return; // a newer query is in flight
			// Filter out the synthetic invite-by-email row — POST /api/drives
			// refuses email subjects (drive Owner must be a real user or group).
			ownerSuggestions = results.filter((r) => r.type === 'user' || r.type === 'group');
		} finally {
			if (token === ownerSearchToken) ownerSearching = false;
		}
	}

	function pickOwner(r: Recipient) {
		driveForm.ownerPick = r;
		driveForm.ownerQuery = r.label;
		ownerSuggestions = [];
	}

	// ── Manage-owners modal (D3a admin bypass) ──────────────────────────────
	// State is null when closed; carries the drive being edited otherwise.
	let manageOwnersDrive = $state<Drive | null>(null);
	let manageOwnersError = $state<string | null>(null);
	let manageOwnersBusy = $state(false);
	// Independent owner-search state so the "manage owners" autocomplete
	// doesn't fight with the create-drive form's autocomplete.
	let manageOwnersQuery = $state('');
	let manageOwnersSuggestions = $state<Recipient[]>([]);
	let manageOwnersSearchToken = 0;
	let manageOwnersSearching = $state(false);

	function openManageOwners(d: Drive) {
		manageOwnersDrive = d;
		manageOwnersError = null;
		manageOwnersQuery = '';
		manageOwnersSuggestions = [];
		// Members were already fetched on tab load; nothing else to do.
	}

	function closeManageOwners() {
		manageOwnersDrive = null;
		manageOwnersError = null;
		manageOwnersQuery = '';
		manageOwnersSuggestions = [];
	}

	async function searchManageOwnersCandidates(q: string) {
		const trimmed = q.trim();
		if (!trimmed) {
			manageOwnersSuggestions = [];
			return;
		}
		const token = ++manageOwnersSearchToken;
		manageOwnersSearching = true;
		try {
			// Admin adding owners — allow self (the share-modal "no
			// self" guard doesn't apply to drive-owner management).
			const results = await searchRecipients(trimmed, { includeSelf: true });
			if (token !== manageOwnersSearchToken) return;
			// Filter out emails (POST admin/members refuses them) and any
			// subject already an Owner of this drive (no point re-adding).
			const currentOwnerIds = new Set(
				(driveMembers[manageOwnersDrive?.id ?? ''] ?? [])
					.filter((m) => m.role === 'owner')
					.map((m) => `${m.subject.type}-${m.subject.id}`)
			);
			manageOwnersSuggestions = results.filter(
				(r) =>
					(r.type === 'user' || r.type === 'group') && !currentOwnerIds.has(`${r.type}-${r.id}`)
			);
		} finally {
			if (token === manageOwnersSearchToken) manageOwnersSearching = false;
		}
	}

	// Pessimistic refetch after every mutation — the membership list is
	// small (a handful of owners) and the alternative (mutating local
	// state) duplicates the server's role-resolution + last-owner logic.
	async function reloadDriveMembers(driveId: string) {
		try {
			driveMembers = {
				...driveMembers,
				[driveId]: await listDriveMembersAdmin(driveId)
			};
		} catch (e) {
			manageOwnersError = errorMessage(e);
		}
	}

	async function addOwner(r: Recipient) {
		if (!manageOwnersDrive || (r.type !== 'user' && r.type !== 'group')) return;
		manageOwnersBusy = true;
		manageOwnersError = null;
		try {
			await addDriveMemberAdmin(manageOwnersDrive.id, { type: r.type, id: r.id }, 'owner');
			manageOwnersQuery = '';
			manageOwnersSuggestions = [];
			await reloadDriveMembers(manageOwnersDrive.id);
		} catch (e) {
			manageOwnersError = errorMessage(e);
		} finally {
			manageOwnersBusy = false;
		}
	}

	async function removeOwner(m: DriveMember) {
		if (!manageOwnersDrive) return;
		const confirmMsg = t('admin.drive_owner_remove_confirm', 'Remove this owner from the drive?');
		if (!(await showConfirm(confirmMsg))) return;
		manageOwnersBusy = true;
		manageOwnersError = null;
		try {
			await removeDriveMemberAdmin(manageOwnersDrive.id, {
				type: m.subject.type,
				id: m.subject.id
			});
			await reloadDriveMembers(manageOwnersDrive.id);
		} catch (e) {
			manageOwnersError = errorMessage(e);
		} finally {
			manageOwnersBusy = false;
		}
	}

	// Re-derive the current owners list inside the modal so it reacts to
	// `driveMembers` changes after add/remove.
	const manageOwnersList = $derived(
		manageOwnersDrive
			? (driveMembers[manageOwnersDrive.id] ?? []).filter(
					(m) => m.role === 'owner' && (m.subject.type === 'user' || m.subject.type === 'group')
				)
			: []
	);

	// ── Manage-policies modal (D5 admin-only mutation) ─────────────────────
	// Policies were owner-mutable in the original D5 design; the carve-out
	// to admin-only fixed the self-policing-soft-cap hole (an owner could
	// disable forbid_external_sharing, share, re-enable — net zero
	// enforcement). The owner UI no longer surfaces policies at all; this
	// modal is the only editor. See `docs/plan/drive.md` §8.
	let managePoliciesDrive = $state<Drive | null>(null);
	let managePoliciesDraft = $state<Required<DrivePoliciesPartial>>({
		forbid_sharing: false,
		forbid_external_sharing: false,
		forbid_public_links: false,
		forbid_cross_drive_move: false,
		forbid_owner_role_change: false,
		// §15 opt-in scope flags. Default personal drives ship with `true`
		// on the wire (materialised by the DB-side create path + backfill
		// migration), so `readPolicyBool` will surface the correct current
		// state on modal open.
		include_in_photo_index: false,
		include_in_music_index: false,
		read_only: false
	});
	let managePoliciesError = $state<string | null>(null);
	let managePoliciesBusy = $state(false);

	function openManagePolicies(d: Drive) {
		managePoliciesDrive = d;
		managePoliciesError = null;
		const p = (d.policies ?? {}) as Record<string, unknown>;
		managePoliciesDraft = {
			forbid_sharing: readPolicyBool(p, 'forbid_sharing'),
			forbid_external_sharing: readPolicyBool(p, 'forbid_external_sharing'),
			forbid_public_links: readPolicyBool(p, 'forbid_public_links'),
			forbid_cross_drive_move: readPolicyBool(p, 'forbid_cross_drive_move'),
			forbid_owner_role_change: readPolicyBool(p, 'forbid_owner_role_change'),
			include_in_photo_index: readPolicyBool(p, 'include_in_photo_index'),
			include_in_music_index: readPolicyBool(p, 'include_in_music_index'),
			read_only: readPolicyBool(p, 'read_only')
		};
	}

	function closeManagePolicies() {
		managePoliciesDrive = null;
		managePoliciesError = null;
	}

	async function saveManagePolicies() {
		if (!managePoliciesDrive) return;
		managePoliciesBusy = true;
		managePoliciesError = null;
		try {
			const merged: DrivePolicies = await updateDrivePolicies(
				managePoliciesDrive.id,
				managePoliciesDraft
			);
			// Refresh the drive row's policies in place so the next time
			// the admin opens this modal they see the persisted state.
			const driveId = managePoliciesDrive.id;
			drivesList = drivesList.map((d) =>
				d.id === driveId ? { ...d, policies: { ...d.policies, ...merged } } : d
			);
			// The shared `drivesStore` (feeds `/config/drive/{uuid}`, the
			// sidebar picker, the breadcrumb) caches `GET /api/drives` with
			// `loaded=true` after the first fetch — without this refresh
			// call the admin's policy change wouldn't propagate to those
			// surfaces until a full page reload. Sibling `requestDeleteDrive`
			// does the same after `deleteDriveAdmin`.
			//
			// Fire-and-forget: the modal closes immediately; the picker
			// re-renders in place when the promise settles a few ms later.
			void drivesStore.refresh();
			closeManagePolicies();
		} catch (e) {
			managePoliciesError = errorMessage(e);
		} finally {
			managePoliciesBusy = false;
		}
	}

	// ─────────────────────────────────────────────────────────────
	// Shared-drive quota edit modal.
	//
	// Personal drives are refused server-side (400) because their
	// effective cap is the owner user's `storage_quota_bytes`
	// envelope (memory `project_user_envelope_quota_model`). The
	// action button in the table doesn't render for personal drives,
	// so this state only feeds shared-drive PATCH calls.
	//
	// The draft (value, unit, unlimited toggle) lives inside the
	// shared <QuotaEditor>; the parent tracks only the subject and
	// current bytes.
	// ─────────────────────────────────────────────────────────────
	let driveQuotaModal = $state<{
		driveId: string;
		driveName: string;
		initialBytes: number | null;
	} | null>(null);
	let driveQuotaError = $state<string | null>(null);
	let driveQuotaBusy = $state(false);

	function openDriveQuota(d: Drive) {
		driveQuotaError = null;
		driveQuotaModal = {
			driveId: d.id,
			driveName: d.name,
			// `quota_bytes` is `number | null | undefined` on the DTO
			// (optional + nullable). Both undefined and null map to
			// unlimited — collapse to null for the modal.
			initialBytes: d.quota_bytes ?? null
		};
	}

	// `unlimited` maps to `null` on the wire for the drive endpoint
	// (backend `Option::None` = "no cap") — distinct from the user
	// envelope, which uses `0`. Both encodings live in their
	// respective save callbacks, keeping <QuotaEditor> endpoint-
	// agnostic.
	async function saveDriveQuota(result: { unlimited: boolean; bytes: number }) {
		if (!driveQuotaModal) return;
		driveQuotaBusy = true;
		driveQuotaError = null;
		try {
			const quota_bytes = result.unlimited ? null : result.bytes;
			const persisted = await updateDriveQuota(driveQuotaModal.driveId, quota_bytes);
			const driveId = driveQuotaModal.driveId;
			drivesList = drivesList.map((d) => (d.id === driveId ? { ...d, quota_bytes: persisted } : d));
			// Sibling surfaces (sidebar picker, breadcrumb) read the
			// cached `GET /api/drives`. Mirrors the policies-modal
			// pattern above.
			void drivesStore.refresh();
			driveQuotaModal = null;
		} catch (e) {
			driveQuotaError = errorMessage(e);
		} finally {
			driveQuotaBusy = false;
		}
	}

	// Policy definitions live in `$lib/utils/drivePolicies` so the same
	// list drives the admin "Manage policies" modal AND the read-only
	// summary on `/config/drive/{uuid}`. Adding a policy is one literal-
	// array push there + one field in `DrivePolicies` in `types.ts`.

	// Admin-driven delete-drive flow (D3b). Guarded by the confirm modal
	// because the action is destructive and irreversible. The backend
	// refuses the default Personal drive (405) and any non-empty drive
	// (409); we surface those as toasts rather than silently swallow.
	async function requestDeleteDrive(d: Drive) {
		const msg = t(
			'admin.drive_delete_confirm',
			{ name: d.name },
			'Delete drive "{{name}}"? This cannot be undone.'
		);
		if (!(await showConfirm(msg))) return;
		try {
			await deleteDriveAdmin(d.id);
			// Refresh the listing + the sidebar picker. Both have a cached
			// view of this drive; without the refresh the row lingers
			// until the next full reload.
			await loadDrivesTab();
			await drivesStore.refresh();
			ui.notify(t('admin.drive_deleted', 'Drive deleted.'), 'success');
		} catch (e) {
			reportError(e);
		}
	}

	async function submitDriveCreate(e: SubmitEvent) {
		e.preventDefault();
		const name = driveForm.name.trim();
		if (name.length === 0) {
			driveCreateError = t('admin.drive_error_name_required', 'Drive name is required.');
			return;
		}
		const owner = driveForm.ownerPick;
		if (!owner || (owner.type !== 'user' && owner.type !== 'group')) {
			driveCreateError = t(
				'admin.drive_error_owner_required',
				'Pick a user or group as the drive owner.'
			);
			return;
		}
		driveCreating = true;
		driveCreateError = null;
		try {
			await createDrive({
				kind: 'shared',
				name,
				owner: { type: owner.type, id: owner.id },
				quota_bytes:
					driveForm.quotaValue > 0 ? Math.round(driveForm.quotaValue * driveForm.quotaUnit) : null
			});
			driveCreateOpen = false;
			await loadDrivesTab();
			// The global drives store backs the sidebar picker; re-fetch it
			// so the new drive shows up for every consumer (picker, breadcrumb,
			// session bootstrap) without a page reload.
			await drivesStore.refresh();
			ui.notify(t('admin.drive_created', 'Drive created.'), 'success');
		} catch (err) {
			driveCreateError = errorMessage(err);
		} finally {
			driveCreating = false;
		}
	}

	async function togglePlugin(p: PluginInfo) {
		try {
			await setPluginEnabled(p.id, !p.enabled);
			await loadPlugins();
		} catch (e) {
			reportError(e);
		}
	}

	async function removePlugin(p: PluginInfo) {
		if (
			!(await showConfirm(
				t('admin.confirm_delete_plugin', { name: p.name }, 'Delete plugin {{name}}?')
			))
		)
			return;
		try {
			await deletePlugin(p.id);
			await loadPlugins();
		} catch (e) {
			reportError(e);
		}
	}

	function changePage(delta: number) {
		const next = pageIndex + delta;
		if (next < 0 || next * PAGE_SIZE >= total) return;
		pageIndex = next;
		void loadUsers();
	}

	// Lazy-load each tab's data on first visit.
	let loaded = $state<Record<Tab, boolean>>({
		dashboard: false,
		users: false,
		sessions: false,
		drives: false,
		mounts: false,
		plugins: false,
		oidc: false,
		storage: false,
		smtp: false,
		jobs: false
	});

	$effect(() => {
		if (loaded[tab]) return;
		loaded[tab] = true;
		if (tab === 'dashboard') void loadDashboard();
		else if (tab === 'users') void loadUsers();
		else if (tab === 'sessions') void loadSessions();
		else if (tab === 'drives') void loadDrivesTab();
		else if (tab === 'mounts') void loadMounts();
		else if (tab === 'plugins') void loadPlugins();
		else if (tab === 'oidc') void loadOidc();
		else if (tab === 'storage') {
			void loadStorage();
			void loadMigration();
		} else if (tab === 'smtp') void loadSmtp();
	});

	// Stop polling when leaving the storage tab / unmounting.
	$effect(() => {
		if (tab !== 'storage') stopMigrationPoll();
		return () => stopMigrationPoll();
	});

	// Tear down the live log stream when leaving plugins / unmounting.
	$effect(() => {
		if (tab !== 'plugins') stopLogStream();
		return () => stopLogStream();
	});
</script>

<svelte:head>
	<title>{t('admin.title', 'Admin')} › {tabLabel} · OxiCloud</title>
</svelte:head>

{#snippet envBadge(on: boolean)}
	{#if on}
		<span class="badge badge--env" title={t('admin.env_locked', 'Set by an environment variable')}
			>ENV</span
		>
	{/if}
{/snippet}

<!--
  The horizontal tab-bar that used to live here was displaced into
  the shared AppShell sidebar (context-aware — swaps to admin
  sections when the URL is under /admin/*). This page now renders
  ONLY the tab content; navigation is via the sidebar + URL. See
  `lib/components/AppShell.svelte` (ADMIN_LINKS).
-->
<main class="admin">
	<!--
	  H1 shows the current section since the sidebar is what
	  communicates which admin area we're in — the plain "Admin"
	  h1 was informationless once the tab bar moved out.
	-->
	<h1>{t('admin.title', 'Admin')} > {tabLabel}</h1>

	{#if tab === 'dashboard'}
		{#if dashboardError}
			<p class="status status--error">{dashboardError}</p>
		{:else if !dashboard}
			<p class="status">{t('common.loading', 'Loading…')}</p>
		{:else}
			<div class="ds-grid">
				<div class="ds-card">
					<span class="ds-num">{dashboard.total_users}</span>{t('admin.total_users', 'Total users')}
				</div>
				<div class="ds-card">
					<span class="ds-num">{dashboard.active_users}</span>{t('admin.active_users', 'Active')}
				</div>
				<div class="ds-card">
					<span class="ds-num">{dashboard.admin_users}</span>{t('admin.admin_users', 'Admins')}
				</div>
				<div class="ds-card">
					<span class="ds-num">v{dashboard.server_version}</span>{t('admin.version', 'Version')}
				</div>
			</div>

			<div class="ds-grid">
				<div class="ds-card">
					<span class="ds-flag" class:ds-flag--on={dashboard.auth_enabled}>
						{dashboard.auth_enabled
							? t('admin.enabled', 'Enabled')
							: t('admin.disabled', 'Disabled')}
					</span>
					{t('admin.auth', 'Authentication')}
				</div>
				<div class="ds-card">
					<span class="ds-flag" class:ds-flag--on={dashboard.oidc_configured}>
						{dashboard.oidc_configured ? t('admin.active', 'Active') : t('admin.off', 'Off')}
					</span>
					{t('admin.oidc', 'OIDC / SSO')}
				</div>
				<div class="ds-card">
					<span class="ds-flag" class:ds-flag--on={dashboard.quotas_enabled}>
						{dashboard.quotas_enabled
							? t('admin.enabled', 'Enabled')
							: t('admin.disabled', 'Disabled')}
					</span>
					{t('admin.quotas', 'Quotas')}
				</div>
			</div>

			{#if dashboard.users_over_quota > 0}
				<div class="card warn-card warn-card--danger">
					<Icon name="exclamation-circle" />
					<div>
						<strong class="ds-num">{dashboard.users_over_quota}</strong>
						{t('admin.over_quota', { n: dashboard.users_over_quota }, '{{n}} users over quota')}
					</div>
				</div>
			{/if}
			{#if dashboard.users_over_80_percent > 0}
				<div class="card warn-card warn-card--warn">
					<Icon name="exclamation-triangle" />
					<div>
						<strong class="ds-num">{dashboard.users_over_80_percent}</strong>
						{t(
							'admin.over_80',
							{ n: dashboard.users_over_80_percent },
							'{{n}} users over 80% quota'
						)}
					</div>
				</div>
			{/if}

			<div class="storage-cards">
				<div class="card storage-cards__quota">
					<h2>{t('admin.quota_usage', 'Quota usage')}</h2>
					<p class="muted storage-cards__hint">
						{t(
							'admin.quota_usage_hint',
							'Pre-dedup, logical file sizes. Includes trashed files until permanent deletion.'
						)}
					</p>
					<table class="quota-table">
						<tbody>
							{#each dashboard.drive_usage ?? [] as row (row.kind)}
								{@const label =
									row.kind === 'personal'
										? t('admin.quota_personal', 'Personal drives')
										: t('admin.quota_shared', 'Shared drives')}
								{@const pct =
									row.capped_quota_bytes && row.capped_quota_bytes > 0
										? (row.used_bytes / row.capped_quota_bytes) * 100
										: null}
								{#if row.capped_count > 0 || row.unlimited_count > 0}
									<tr>
										<th scope="row">{label}</th>
										<td class="quota-table__num">
											{#if row.capped_quota_bytes !== null && pct !== null}
												{formatBytes(row.used_bytes)} / {formatBytes(row.capped_quota_bytes)}
												<span class="quota-table__pct">({pct.toFixed(1)}%)</span>
											{:else}
												{formatBytes(row.used_bytes)}
											{/if}
										</td>
										<td class="quota-table__bar">
											{#if pct !== null}
												<div class="ds-bar">
													<div
														class="ds-fill"
														class:ds-fill--warn={pct > 70}
														class:ds-fill--danger={pct > 90}
														style:width="{Math.min(pct, 100)}%"
													></div>
												</div>
											{/if}
										</td>
										<td class="quota-table__meta">
											{#if row.unlimited_count > 0}
												<span class="quota-table__unlimited">
													{t(
														'admin.quota_unlimited',
														{ n: row.unlimited_count },
														'{{n}} unlimited'
													)}
												</span>
											{/if}
										</td>
									</tr>
								{/if}
							{/each}
						</tbody>
					</table>
				</div>

				<div class="card storage-cards__backend">
					<h2>{t('admin.backend_storage', 'Backend storage')}</h2>
					{#if dashboard.total_bytes_stored !== undefined}
						<dl class="storage-cards__stats">
							<div>
								<dt>{t('admin.backend_stored', 'Stored')}</dt>
								<dd>{formatBytes(dashboard.total_bytes_stored)}</dd>
							</div>
							<div
								class="storage-cards__stat-hint"
								title={t(
									'admin.backend_referenced_hint',
									'Sum of blob references (size × ref_count). Can exceed the drive total because thumbnails, derived assets, and blobs pending garbage collection still hold references.'
								)}
							>
								<dt>
									{t('admin.backend_referenced', 'Referenced')}
									<Icon name="info-circle" />
								</dt>
								<dd>
									{formatBytes(
										Math.round((dashboard.total_bytes_stored ?? 0) * (dashboard.dedup_ratio ?? 1))
									)}
								</dd>
							</div>
							<div>
								<dt>{t('admin.backend_dedup_ratio', 'Dedup ratio')}</dt>
								<dd>
									{dashboard.dedup_ratio !== undefined
										? `${dashboard.dedup_ratio.toFixed(2)}×`
										: '—'}
								</dd>
							</div>
						</dl>
					{:else}
						<p class="muted">—</p>
					{/if}
				</div>
			</div>

			{#if dashboard.registration_enabled !== undefined}
				<div class="card">
					<h2>{t('admin.registration', 'Registration')}</h2>
					<label class="checkbox">
						<input
							type="checkbox"
							data-testid="admin-dashboard-registration-checkbox"
							checked={dashboard.registration_enabled}
							onchange={(e) => toggleRegistration(e.currentTarget.checked)}
						/>
						<span>{t('admin.allow_registration', 'Allow public user registration')}</span>
					</label>
					{#if !dashboard.registration_enabled}
						<p class="alert alert--warn registration-warning">
							<Icon name="exclamation-triangle" />
							{t(
								'admin.registration_disabled_warning',
								'Public registration is disabled. Only admins can create new accounts.'
							)}
						</p>
					{/if}
				</div>
			{/if}

			<div class="card">
				<h2>{t('admin.maintenance', 'Maintenance')}</h2>
				<p class="muted">
					{t(
						'admin.maintenance_hint',
						'Re-scan existing files to backfill metadata. Safe to re-run; processes the whole library and may take a while.'
					)}
				</p>
				<div class="maint-row">
					<button class="btn btn-secondary" disabled={audioBusy} onclick={runAudioReindex}>
						<Icon name="music" />
						{audioBusy
							? t('admin.running', 'Running…')
							: t('admin.reextract_audio', 'Re-extract audio metadata')}
					</button>
					{#if audioResult}
						<span class="muted maint-result">
							{t(
								'admin.reextract_done',
								{
									processed: audioResult.processed,
									total: audioResult.total,
									failed: audioResult.failed
								},
								'{{processed}}/{{total}} processed · {{failed}} failed'
							)}
						</span>
					{/if}
				</div>
				<div class="maint-row">
					<button class="btn btn-secondary" disabled={photoBusy} onclick={runPhotoReindex}>
						<Icon name="images" />
						{photoBusy
							? t('admin.running', 'Running…')
							: t('admin.reextract_photos', 'Re-extract photo & video capture dates')}
					</button>
					{#if photoResult}
						<span class="muted maint-result">
							{t(
								'admin.reextract_done',
								{
									processed: photoResult.processed,
									total: photoResult.total,
									failed: photoResult.failed
								},
								'{{processed}}/{{total}} processed · {{failed}} failed'
							)}
						</span>
					{/if}
				</div>
			</div>
		{/if}
	{:else if tab === 'oidc'}
		<div class="card">
			<h2>{t('admin.oidc', 'OIDC / SSO')}</h2>
			{#if !oidc}
				<p class="status">{t('common.loading', 'Loading…')}</p>
			{:else}
				<form
					class="form"
					data-testid="admin-oidc-form"
					onsubmit={(e) => (e.preventDefault(), doSaveOidc())}
				>
					<label class="checkbox">
						<input
							type="checkbox"
							data-testid="admin-oidc-enabled-checkbox"
							bind:checked={oidc.enabled}
						/>
						<span>{t('admin.oidc_enabled', 'Enable OIDC login')}</span>
					</label>
					<label
						><span
							>{t('admin.oidc_issuer', 'Issuer URL')}{@render envBadge(
								isEnvLocked(oidc.env_overrides, 'issuer_url')
							)}</span
						>
						<input
							bind:value={oidc.issuer_url}
							data-testid="admin-oidc-issuer-input"
							placeholder="https://idp.example.com"
							disabled={isEnvLocked(oidc.env_overrides, 'issuer_url')}
						/></label
					>
					<button
						type="button"
						class="btn btn-secondary"
						data-testid="admin-oidc-discover-btn"
						onclick={runOidcTest}
					>
						<Icon name="search" />
						{t('admin.oidc_discover', 'Test / discover')}
					</button>
					{#if oidcTest}
						<div
							class="discovery-result {oidcTest.success
								? 'discovery-result--ok'
								: 'discovery-result--fail'}"
						>
							<strong>
								<Icon name={oidcTest.success ? 'check-circle' : 'times-circle'} />
								{oidcTest.message}
							</strong>
							{#if oidcTest.success}
								<dl class="kv">
									<dt>{t('admin.oidc_issuer', 'Issuer URL')}</dt>
									<dd>{oidcTest.issuer || '—'}</dd>
									<dt>{t('admin.oidc_auth_endpoint', 'Auth endpoint')}</dt>
									<dd>{oidcTest.authorization_endpoint || '—'}</dd>
								</dl>
							{/if}
						</div>
					{/if}
					<label
						><span
							>{t('admin.oidc_client_id', 'Client ID')}{@render envBadge(
								isEnvLocked(oidc.env_overrides, 'client_id')
							)}</span
						>
						<input
							bind:value={oidc.client_id}
							data-testid="admin-oidc-client-id-input"
							disabled={isEnvLocked(oidc.env_overrides, 'client_id')}
						/></label
					>
					<label
						><span
							>{t('admin.oidc_client_secret', 'Client secret')}{@render envBadge(
								isEnvLocked(oidc.env_overrides, 'client_secret')
							)}</span
						>
						<input
							type="password"
							data-testid="admin-oidc-client-secret-input"
							bind:value={oidc.client_secret}
							disabled={isEnvLocked(oidc.env_overrides, 'client_secret')}
							placeholder={oidc.client_secret_set
								? t('admin.unchanged', 'Leave blank to keep current')
								: ''}
						/>
						{#if oidc.client_secret_set}
							<span class="secret-hint">
								<Icon name="check-circle" />
								{t('admin.oidc_secret_set', 'A client secret is already configured.')}
							</span>
						{/if}</label
					>
					<label
						><span
							>{t('admin.oidc_scopes', 'Scopes')}{@render envBadge(
								isEnvLocked(oidc.env_overrides, 'scopes')
							)}</span
						>
						<input
							bind:value={oidc.scopes}
							data-testid="admin-oidc-scopes-input"
							placeholder="openid profile email"
							disabled={isEnvLocked(oidc.env_overrides, 'scopes')}
						/></label
					>
					<label
						><span
							>{t('admin.oidc_provider_name', 'Provider name')}{@render envBadge(
								isEnvLocked(oidc.env_overrides, 'provider_name')
							)}</span
						>
						<input
							bind:value={oidc.provider_name}
							data-testid="admin-oidc-provider-name-input"
							disabled={isEnvLocked(oidc.env_overrides, 'provider_name')}
						/></label
					>
					<label
						><span
							>{t('admin.oidc_admin_groups', 'Admin groups')}{@render envBadge(
								isEnvLocked(oidc.env_overrides, 'admin_groups')
							)}</span
						>
						<input
							bind:value={oidc.admin_groups}
							data-testid="admin-oidc-admin-groups-input"
							disabled={isEnvLocked(oidc.env_overrides, 'admin_groups')}
						/></label
					>
					<label class="checkbox">
						<input
							type="checkbox"
							data-testid="admin-oidc-auto-provision-checkbox"
							bind:checked={oidc.auto_provision}
						/>
						<span>{t('admin.oidc_auto_provision', 'Auto-provision users on first login')}</span>
					</label>
					<label class="checkbox">
						<input
							type="checkbox"
							data-testid="admin-oidc-disable-pw-checkbox"
							bind:checked={oidc.disable_password_login}
						/>
						<span>{t('admin.oidc_disable_pw', 'Disable password login (OIDC only)')}</span>
					</label>
					{#if oidc.callback_url}
						<p class="muted callback-row">
							{t('admin.oidc_callback', 'Callback URL')}: <code>{oidc.callback_url}</code>
							<button
								type="button"
								class="btn btn-sm btn-secondary"
								data-testid="admin-oidc-callback-copy-btn"
								onclick={() => copyText(oidc?.callback_url ?? '')}
							>
								<Icon name="copy" />
								{t('common.copy', 'Copy')}
							</button>
						</p>
					{/if}
					{#if oidcMsg}<p class={oidcMsg.ok ? 'status--ok' : 'status--error'}>
							{oidcMsg.text}
						</p>{/if}
					<button
						class="btn btn-primary"
						type="submit"
						data-testid="admin-oidc-save-btn"
						disabled={oidcSaving}
					>
						{t('common.save', 'Save')}
					</button>
				</form>
			{/if}
		</div>
	{:else if tab === 'storage'}
		<!-- ══════════════════════════════════════════════════════════════
		     STORAGE TAB — rewritten for the multi-entry model
		     (see docs/plan/storage-multi-entry.md).

		     Design: no save form. .env is the only place backends are
		     declared. This tab is a read-only view of what booted plus
		     the per-entry actions an admin actually needs — test,
		     audit via blobs_consistency, and migrate+activate to a
		     non-active entry.

		     The legacy form + related handlers/state live in git
		     history; deleted here in one sweep.
		     ══════════════════════════════════════════════════════════ -->
		<!-- Section 1 — Content store: global DB blob stats,
		     independent of any backend entry. Rendered first because
		     it's the "what's actually in the system" answer;
		     Storage backend + Encryption below are the "where /
		     how it's stored" answers. -->
		{#if storage}
			<section
				class="card storage-content-stats"
				data-testid="admin-storage-content-stats"
				aria-labelledby="admin-storage-content-stats-title"
			>
				<h2 id="admin-storage-content-stats-title">
					{t('admin.storage_content_stats_title', 'Content store')}
				</h2>
				<p class="muted storage-content-stats__hint">
					{t(
						'admin.storage_content_stats_hint',
						'Aggregate over the DB blob store — independent of which backend entry holds the bytes.'
					)}
				</p>
				<dl class="storage-content-stats__grid">
					<div>
						<dt>{t('admin.storage_blobs', 'Blobs')}</dt>
						<dd>{storage.total_blobs ?? '—'}</dd>
					</div>
					<div>
						<dt>{t('admin.storage_size', 'Stored')}</dt>
						<dd>
							{storage.total_bytes_stored != null ? formatBytes(storage.total_bytes_stored) : '—'}
						</dd>
					</div>
					<div>
						<dt>{t('admin.storage_dedup', 'Dedup ratio')}</dt>
						<dd>
							{storage.dedup_ratio != null ? `${storage.dedup_ratio.toFixed(2)}x` : '—'}
						</dd>
					</div>
				</dl>
			</section>
		{/if}

		<!-- Section 2 — Storage backend: per-entry cards (backend
		     type, location, actions). Encryption pair-chain moved
		     out to its own section below. -->
		<div class="card">
			<h2>{t('admin.storage_title', 'Storage backend')}</h2>
			<p class="muted">
				{t(
					'admin.storage_move_hint',
					'To move to another backend storage: declare a new entry in your `.env` (keep the current one), restart the server so it picks it up, then trigger a migration from this page. Cutover happens automatically when the copy completes — no second restart needed.'
				)}
			</p>
			{#if !storage}
				<p class="status">{t('common.loading', 'Loading…')}</p>
			{:else if !storage.entries || storage.entries.length === 0}
				<!-- Legacy zero-entries path — the parser synthesises a
				     single `default` entry from flat vars and emits a
				     deprecation warning. Show the same warning here so
				     admins with old .env files see it in the UI too. -->
				<p class="alert alert--warn">
					<Icon name="exclamation-triangle" />
					{t(
						'admin.storage_no_entries',
						{ backend: storage.current_backend ?? '?' },
						'No OXICLOUD_STORAGE_ENTRIES declared. Running on the legacy single-backend fallback ({{backend}}). Migrate to the multi-entry model — see docs/config/env.md.'
					)}
				</p>
				<dl class="kv">
					<dt>{t('admin.storage_current', 'Current backend')}</dt>
					<dd>{storage.current_backend ?? '—'}</dd>
					<dt>{t('admin.storage_blobs', 'Blobs')}</dt>
					<dd>{storage.total_blobs ?? '—'}</dd>
					<dt>{t('admin.storage_size', 'Stored')}</dt>
					<dd>
						{storage.total_bytes_stored != null ? formatBytes(storage.total_bytes_stored) : '—'}
					</dd>
					<dt>{t('admin.storage_dedup', 'Dedup ratio')}</dt>
					<dd>{storage.dedup_ratio != null ? `${storage.dedup_ratio.toFixed(2)}x` : '—'}</dd>
				</dl>
			{:else}
				<!-- Banner keys off `serverStatus().readonly` (live-updated
				     via `x-server-status` on every API response) rather
				     than `storage.migration_readonly` (a snapshot from
				     the one-shot `getStorageSettings()` fetch). Prevents
				     the "stale until force-refresh" bug when navigating
				     into /admin/storage while a migration is running:
				     any API call that fires on tab entry — even
				     `loadStorage` itself — updates the store from the
				     response header, so the banner shows within the
				     first render cycle. -->
				{#if serverStatus().readonly}
					<div
						class="cutover-hint cutover-hint--readonly"
						data-testid="admin-migration-readonly-banner"
					>
						<h3>
							<Icon name="lock" />
							{t('admin.mig_readonly_title', 'Server in migration read-only mode')}
						</h3>
						<p class="cutover-hint__readonly-body">
							{t(
								'admin.mig_readonly_body',
								'All writes (upload, rename, delete, share) are refused until the migration completes. Reads (browse, download) are unaffected. When the copy finishes the server switches to the new backend automatically — no restart needed.'
							)}
						</p>
					</div>
				{/if}

				<!-- Card-per-entry layout — most installs have 1 backend
				     (occasionally 2 during a migration), so a rich card
				     reads better than a wide table. Active entry gets
				     a highlight ring. Migrate & activate is per-card
				     and only shown on non-active cards when no other
				     migration is in flight. -->
				{@const migrationInFlight =
					migration != null && (migration.status === 'running' || migration.status === 'paused')}
				<div class="entries-list" data-testid="admin-storage-entries-list">
					{#each storage.entries as entry (entry.name)}
						{@const test = entryTest[entry.name]}
						<article
							class="entry-card"
							class:entry-card--active={entry.is_active}
							data-testid={`admin-storage-entry-${entry.name}`}
						>
							<header class="entry-card__header">
								<div class="entry-card__title">
									<code class="entry-card__name">{entry.name}</code>
									{#if entry.is_active}
										<span class="entry-card__badge entry-card__badge--active">
											<Icon name="check-circle" />
											{t('admin.entry_active', 'active')}
										</span>
									{:else}
										<span class="entry-card__badge">
											{t('admin.entry_inactive', 'available')}
										</span>
									{/if}
									{#if entry.encryption_enabled}
										<span
											class="entry-card__badge entry-card__badge--encrypted"
											title="AES-256-GCM"
										>
											<Icon name="lock" /> AES-256
										</span>
									{/if}
								</div>
								<!-- Fixed three-slot action row so buttons line up
								     vertically across cards regardless of which
								     slots are active. The Migrate slot is
								     rendered but visibility-hidden on the
								     currently-active entry (no sense
								     migrating to yourself) and while another
								     migration is in flight (the handler would
								     refuse anyway). `visibility: hidden`
								     keeps the layout box; `aria-hidden` +
								     `tabindex=-1` remove it from
								     keyboard/screen-reader navigation. -->
								<div class="entry-card__actions">
									<button
										type="button"
										class="btn btn-sm btn-secondary entry-card__action-btn"
										disabled={test?.busy ?? false}
										data-testid={`admin-storage-test-${entry.name}`}
										onclick={() => doTestEntry(entry.name)}
									>
										<Icon name="vial" />
										{test?.busy
											? t('admin.storage_testing', 'Testing…')
											: t('admin.storage_test', 'Test')}
									</button>
									<button
										type="button"
										class="btn btn-sm btn-secondary entry-card__action-btn"
										data-testid={`admin-storage-audit-${entry.name}`}
										onclick={() => doAuditEntry(entry.name)}
									>
										<Icon name="check-double" />
										{t('admin.storage_audit', 'Blob consistency')}
									</button>
									<!-- Storage-side consistency (K4): the mirror of Blob
									     consistency. `blobs_consistency` walks the DB and
									     checks the backend has each blob; `backend_consistency`
									     walks the backend and checks the DB has each hash.
									     Together they close the reference graph. -->
									<button
										type="button"
										class="btn btn-sm btn-secondary entry-card__action-btn"
										data-testid={`admin-storage-backend-audit-${entry.name}`}
										onclick={() => doStorageConsistency(entry.name)}
									>
										<Icon name="database" />
										{t('admin.storage_backend_audit', 'Backend consistency')}
									</button>
									{#if !entry.is_active && !migrationInFlight}
										<button
											type="button"
											class="btn btn-sm btn-primary entry-card__action-btn"
											data-testid={`admin-storage-migrate-${entry.name}`}
											onclick={() => doMigrateActivate(entry.name)}
										>
											<Icon name="crown" />
											{t('admin.storage_migrate_activate', 'Migrate & activate')}
										</button>
									{:else}
										<button
											type="button"
											class="btn btn-sm btn-primary entry-card__action-btn entry-card__action-btn--placeholder"
											aria-hidden="true"
											tabindex={-1}
											disabled
										>
											<Icon name="crown" />
											{t('admin.storage_migrate_activate', 'Migrate & activate')}
										</button>
									{/if}
									<!-- Rotate encryption key (K4 storage-key-rotation).
									     ACTIVE ENTRY ONLY — `storage.blobs` describes the
									     active backend; rotating a non-active entry would
									     produce a `rotation_failed` finding per blob that
									     isn't there (backend refuses this with a 400 too).
									     Placeholder slot on non-active cards keeps the
									     three-button row aligned across the grid. Also
									     disabled while any migration is in flight — backend
									     refuses concurrent encryption-touching jobs. -->
									{#if entry.is_active}
										<button
											type="button"
											class="btn btn-sm btn-secondary entry-card__action-btn"
											disabled={migrationInFlight}
											data-testid={`admin-storage-rotate-${entry.name}`}
											onclick={() => doRotateEntry(entry.name)}
											title={migrationInFlight
												? t(
														'admin.backend_rotate_disabled_migration',
														'Cannot rotate while a migration is in flight.'
													)
												: t(
														'admin.backend_rotate_tooltip',
														'Normalise every blob on this entry to the head pair’s format (upgrade legacy blobs, re-encrypt under a new key, etc.).'
													)}
										>
											<Icon name="key" />
											{t('admin.backend_rotate', 'Rotate key')}
										</button>
									{:else}
										<button
											type="button"
											class="btn btn-sm btn-secondary entry-card__action-btn entry-card__action-btn--placeholder"
											aria-hidden="true"
											tabindex={-1}
											disabled
										>
											<Icon name="key" />
											{t('admin.backend_rotate', 'Rotate key')}
										</button>
									{/if}
								</div>
							</header>
							<dl class="entry-card__grid">
								<dt>{t('admin.entry_backend', 'Backend')}</dt>
								<dd>{entry.backend}</dd>
								<dt>{t('admin.entry_location', 'Location')}</dt>
								<dd class="entry-card__mono">{entry.location_hint ?? '—'}</dd>
							</dl>
							<!-- Pair-list chain — one row per configured pair,
							     head marked. Empty state (no `_ENCRYPTION_KEY`
							     declared at all) hides the whole block; a single
							     `none:` pair renders as one row so admins can see
							     "yes, encryption declaration exists but head is
							     plaintext" vs "no encryption declared". -->
							{#if entry.encryption_pairs?.length}
								<section class="entry-card__pairs" aria-label="Encryption keys">
									<h4 class="entry-card__pairs-title">
										{t('admin.storage_pair_list', 'Encryption keys')}
									</h4>
									<ol class="entry-card__pair-chain">
										{#each entry.encryption_pairs as pair, i (i)}
											<li class="entry-card__pair" class:entry-card__pair--head={pair.is_head}>
												<span class="entry-card__pair-idx">key{i + 1}:</span>
												<span class="entry-card__pair-cipher">{pair.cipher}</span>
												<code class="entry-card__pair-fp">
													{pair.fingerprint ?? '—'}
												</code>
												{#if pair.is_head}
													<span class="entry-card__pair-head-badge">
														{t('admin.storage_pair_head', 'head')}
													</span>
												{/if}
											</li>
										{/each}
									</ol>
									<p class="entry-card__pairs-help muted">
										{t(
											'admin.storage_pair_help',
											'Head is the write key. After a successful rotation with 0 failures, any non-head key can be safely removed from `.env`.'
										)}
									</p>
								</section>
							{/if}
							{#if test?.result != null || test?.error != null}
								<footer class="entry-card__test-result">
									{#if test.error}
										<span class="status--error"><Icon name="times-circle" /> {test.error}</span>
									{:else if test.result}
										{@const ok = test.result.connected ?? false}
										{@const rt = test.result.roundtrip_elapsed_ms}
										{@const cleanup = test.result.cleanup_ok}
										<span class={ok ? 'status--ok' : 'status--error'}>
											<Icon name={ok ? 'check-circle' : 'times-circle'} />
											{ok
												? t('admin.storage_test_success', 'Read/write OK')
												: t('admin.storage_test_failure', 'Test failed')}
											{#if rt != null}
												· {t('admin.storage_test_elapsed', { ms: rt }, '{{ms}} ms')}
											{/if}
											{#if cleanup === false}
												· ⚠ {t('admin.storage_test_cleanup_warn', 'cleanup DELETE failed')}
											{/if}
											{#if !ok}
												— {test.result.message}
											{/if}
										</span>
									{/if}
								</footer>
							{/if}
						</article>
					{/each}
				</div>

				<!-- Migration status line + inline pause/resume. Start is
				     per-row (Migrate & activate button) so no separate
				     Start control here. -->
				<div class="mig-status">
					<p class="muted">
						{t('admin.mig_status', 'Migration status')}:
						<strong>{migration?.status ?? '—'}</strong>
						{#if migration?.status === 'running'}
							<button
								type="button"
								class="btn btn-sm btn-secondary"
								data-testid="admin-migration-pause-btn"
								onclick={() => doMigration('pause')}
							>
								{t('admin.mig_pause', 'Pause')}
							</button>
						{/if}
						{#if migration?.status === 'paused'}
							<button
								type="button"
								class="btn btn-sm btn-primary"
								data-testid="admin-migration-resume-btn"
								onclick={() => doMigration('resume')}
							>
								{t('admin.mig_resume', 'Resume')}
							</button>
						{/if}
					</p>
					{#if migration && migration.total_blobs > 0}
						<div class="ds-bar">
							<div class="ds-fill" style:width="{migrationPct}%"></div>
						</div>
						<p class="muted">
							{migration.migrated_blobs} / {migration.total_blobs} ({migrationPct}%)
							{#if migrationEtaMin != null}
								· {t(
									'admin.mig_eta',
									{ min: migrationEtaMin },
									`~${migrationEtaMin} min remaining`
								)}
							{/if}
						</p>
					{/if}
					{#if migration?.failed_blobs && migration.failed_blobs.length > 0}
						<details class="mig-failed">
							<summary>
								{t(
									'admin.mig_failed',
									{ n: migration.failed_blobs.length },
									`${migration.failed_blobs.length} failed blobs`
								)}
							</summary>
							<pre class="mig-failed__list">{migration.failed_blobs.join('\n')}</pre>
						</details>
					{/if}
				</div>

				<!-- Post-Completed "restart to switch" hint retired in
				     the hot-swap slice. Cutover is now automatic — the
				     migration handler swaps the runtime backend and
				     drops read-only in the same step, so there's
				     nothing left for the operator to do after
				     Completed. -->
			{/if}
			{#if storageMsg}
				<p class={storageMsg.ok ? 'status--ok' : 'status--error'}>{storageMsg.text}</p>
			{/if}
		</div>

		<div class="card">
			<h2>{t('admin.encryption', 'Encryption')}</h2>
			<p class="muted">
				{t(
					'admin.encryption_hint',
					'Generate an AES-256 key for at-rest blob encryption. Set it as OXICLOUD_STORAGE_<name>_ENCRYPTION_KEY under an entry declared in OXICLOUD_STORAGE_ENTRIES — presence of the key implies encryption is enabled on that entry (no separate flag).'
				)}
			</p>
			<button class="btn btn-secondary" disabled={keyBusy} onclick={runGenerateKey}>
				<Icon name="key" />
				{keyBusy ? t('admin.running', 'Running…') : t('admin.gen_key', 'Generate key')}
			</button>
			{#if generatedKey}
				<p class="callback-row">
					<code>{generatedKey.key}</code>
					<button
						type="button"
						class="btn btn-sm btn-secondary"
						onclick={() => copyText(generatedKey?.key ?? '')}
					>
						<Icon name="copy" />
						{t('common.copy', 'Copy')}
					</button>
				</p>
				<p class="muted gen-key-fp">
					{t('admin.gen_key_fingerprint', 'Fingerprint')}:
					<code>{generatedKey.fingerprint}</code>
					<span class="muted">
						—
						{t(
							'admin.gen_key_fingerprint_hint',
							'appears in the boot log and the pair chain above once loaded from `.env`.'
						)}
					</span>
				</p>
				<p class="alert alert--warn">
					<Icon name="exclamation-triangle" />
					{t(
						'admin.gen_key_warning',
						'Store this key securely. If it is lost, the encrypted data is irrecoverably lost.'
					)}
				</p>
			{/if}
		</div>
	{:else if tab === 'smtp'}
		<div class="card">
			<h2>{t('admin.smtp_status', 'SMTP status')}</h2>
			{#if !smtp}
				<p class="status">{t('common.loading', 'Loading…')}</p>
			{:else}
				<dl class="kv">
					<dt>{t('admin.smtp_enabled', 'Enabled')}</dt>
					<dd>{smtp.enabled ? t('common.yes', 'Yes') : t('common.no', 'No')}</dd>
					<dt>{t('admin.smtp_host', 'Host')}</dt>
					<dd>{smtp.host || '—'}</dd>
					<dt>{t('admin.smtp_port', 'Port')}</dt>
					<dd>{smtp.port || '—'}</dd>
					<dt>TLS</dt>
					<dd>{smtp.tls || '—'}</dd>
					<dt>{t('admin.smtp_from', 'From')}</dt>
					<dd>{smtp.from || '—'}</dd>
					<dt>{t('admin.smtp_user_state', 'Auth')}</dt>
					<dd>{smtp.user_state || '—'}</dd>
				</dl>
			{/if}
		</div>
		<div class="card">
			<h2>{t('admin.smtp_test', 'Send test email')}</h2>
			<div class="smtp-test">
				<input
					type="email"
					data-testid="admin-smtp-to-input"
					bind:value={smtpTo}
					placeholder={t('admin.smtp_to', 'recipient@example.com')}
				/>
				<button
					class="btn btn-primary"
					data-testid="admin-smtp-send-btn"
					disabled={smtpSending}
					onclick={runSmtpTest}
				>
					<Icon name="paper-plane" />
					{smtpSending ? t('admin.smtp_sending', 'Sending…') : t('admin.smtp_send', 'Send')}
				</button>
			</div>
			{#if smtpResult}
				{#if smtpResult.success}
					<p class="status--ok">
						<strong>{t('admin.smtp_sent', 'Test email sent.')}</strong><br />
						{t('admin.smtp_server_code', 'Server replied')}:
						<code>{smtpResult.code ?? ''} {smtpResult.message ?? ''}</code>
					</p>
				{:else}
					<p class="status--error">
						<strong>{t('admin.smtp_fail', 'Send failed.')}</strong><br />
						<code
							>{smtpResult.error || smtpResult.message || t('common.error', 'unknown error')}</code
						>
					</p>
				{/if}
			{/if}
		</div>
	{:else if tab === 'users'}
		<div class="bar">
			<button
				class="btn btn--primary"
				data-testid="admin-users-create-btn"
				onclick={() => (createOpen = true)}
			>
				<Icon name="user-plus" />
				{t('admin.create_user', 'Create user')}
			</button>
		</div>
		{#if usersError}
			<p class="status status--error">{usersError}</p>
		{:else}
			<table class="table">
				<thead>
					<tr>
						<th>{t('admin.user', 'User')}</th>
						<th>{t('admin.role', 'Role')}</th>
						<th>{t('admin.auth', 'Auth')}</th>
						<th>{t('admin.status', 'Status')}</th>
						<th>{t('admin.quota', 'Storage usage')}</th>
						<th>{t('admin.last_login', 'Last login')}</th>
						<th></th>
					</tr>
				</thead>
				<tbody>
					{#each users as u (u.id)}
						{@const pct = quotaPct(u)}
						<tr>
							<td>
								<div class="user-vignette-cell">
									<UserVignette
										userId={u.id}
										fallbackLabel={u.username || u.email}
										fallbackSublabel={u.email}
									/>
									{#if isSelf(u)}
										<span class="badge badge--self">{t('admin.you_badge', 'you')}</span>
									{/if}
								</div>
							</td>
							<td>
								<!-- Two badges (role + optional external) live in this
								     cell. `.role-badges` prevents them from splitting
								     across lines when the column narrows: nowrap + a
								     tiny gap keeps them shoulder-to-shoulder, and each
								     badge is `white-space: nowrap` so the badge label
								     itself never wraps mid-word either. -->
								<div class="role-badges">
									<span class="badge badge--{u.role === 'admin' ? 'admin' : 'user'}">
										{#if u.role === 'admin'}<Icon name="shield-alt" />{/if}
										{u.role}
									</span>
									{#if u.is_external}
										<!-- Origin flag, orthogonal to `role`. Grant-only
										     accounts (magic-link / OCM) can never be admin
										     (DB CHECK `users_external_not_admin`) so the two
										     badges never collide in practice — but the
										     "External" badge stacks after the role badge so a
										     future rules change wouldn't hide either signal. -->
										<span
											class="badge badge--external"
											title={t(
												'admin.external_user_hint',
												'Grant-only account (magic-link or OCM). Cannot be admin and has no storage envelope.'
											)}
										>
											<Icon name="building-circle-xmark" />
											{t('admin.external_user', 'external')}
										</span>
									{/if}
								</div>
							</td>
							<!-- Auth cell — capped width. The OIDC provider label is
							     free-form ("keycloak", "google-workspace", …) and
							     drives the column arbitrarily wide when localized
							     column headers ("Authentication" → "Authentification"
							     in fr) also demand room. `.auth-cell` truncates the
							     badge label with an ellipsis so the column stays
							     narrow; the full provider stays reachable via the
							     tooltip on the badge. -->
							<td class="auth-cell">
								<!--
									Auth-capability chip set — ADMIN-ONLY (fields
									scoped to `AdminUserSummaryDto`; never on
									`UserDto`). Any user carries ZERO OR MORE of:
									  * SSO/OIDC — `federation_kind === 'oidc'`,
									    identity delegated to the IdP; label is
									    the provider name.
									  * password — `has_password` — server has a
									    verifiable password on file. Silent
									    migration adds `envelope` alongside.
									  * OPAQUE — migrated=true (green) or
									    envelope=true (info shade, waiting for
									    first OPAQUE login).
									  * mail — user has NONE of the
									    above (no oidc, no password, no
									    envelope). Magic-link-only login. This
									    is the default for externals (grant-only
									    recipients) and for pre-signup accounts
									    awaiting their welcome magic-link.
								-->
								{#if isOidcUser(u)}
									<span class="badge badge--oidc" title={u.federation_issuer}>
										<Icon name="shield-alt" />
										<span class="badge__label">oidc</span>
									</span>
								{/if}
								{#if u.has_password}
									<span
										class="badge badge--password"
										title={t(
											'admin.has_password_title',
											'User has a password on file (legacy or admin-set)'
										)}
									>
										<Icon name="key" />
										<span class="badge__label">{t('admin.auth_password', 'password')}</span>
									</span>
								{/if}
								{#if u.opaque_migrated}
									<span
										class="badge badge--opaque"
										title={t(
											'admin.opaque_migrated_title',
											'User has completed at least one OPAQUE login'
										)}
									>
										<Icon name="shield-alt" />
										<span class="badge__label">OPAQUE</span>
									</span>
								{:else if u.opaque_registered}
									<span
										class="badge badge--opaque-registered"
										title={t(
											'admin.opaque_registered_title',
											'OPAQUE envelope on file — waiting for the user to complete an OPAQUE login'
										)}
									>
										<Icon name="shield-alt" />
										<span class="badge__label">{t('admin.opaque_envelope', 'envelope')}</span>
									</span>
								{/if}
								{#if !isOidcUser(u) && !u.has_password && !u.opaque_registered}
									<!--
										`mail` chip — user's only login path is a
										magic-link to their mailbox. Renamed from
										`passwordless` (2026-08-05) because "mail"
										names the credential channel the operator
										actually cares about (does the user need
										email access to log in? yes) rather than
										the absence of another one.
									-->
									<span
										class="badge badge--mail"
										title={t(
											'admin.mail_login_title',
											'No password, no OPAQUE envelope, no SSO — user logs in via magic-link only'
										)}
									>
										<Icon name="envelope" />
										<span class="badge__label">{t('admin.auth_mail', 'mail')}</span>
									</span>
								{/if}
							</td>
							<td>
								<span class="badge badge--{u.active ? 'active' : 'inactive'}">
									{u.active ? t('admin.active', 'Active') : t('admin.inactive', 'Inactive')}
								</span>
							</td>
							<td>
								{#if u.is_external}
									<!-- External accounts have no storage envelope by
									     design (DB CHECK `users_external_no_storage`
									     enforces storage_quota_bytes = 0). Rendering the
									     usage bar with `0 / 0` reads as "over quota"
									     visually and is misleading; show an em-dash
									     instead. -->
									<span
										class="muted"
										title={t(
											'admin.no_storage_for_external',
											'External accounts have no storage envelope.'
										)}>—</span
									>
								{:else}
									<div class="quota-cell">
										<div class="quota-bar">
											<div
												class="quota-fill"
												class:quota-fill--warn={pct > 70}
												class:quota-fill--danger={pct > 90}
												style:width="{Math.min(pct, 100)}%"
											></div>
										</div>
										<span class="muted">
											{formatBytes(u.storage_used_bytes)} / {u.storage_quota_bytes > 0
												? formatBytes(u.storage_quota_bytes)
												: '∞'}
										</span>
									</div>
								{/if}
							</td>
							<td class="muted">{timeAgo(u.last_login_at)}</td>
							<td>
								<!-- Fixed 5-column grid keeps icons aligned across
								     rows even when a row's user kind skips some
								     actions (external users have no envelope so
								     no quota edit, and no password so no reset;
								     internals never get a promote). Inapplicable
								     actions render as invisible placeholders. -->
								<div class="actions actions--user">
									<!-- Slot 1: quota (internal) OR promote (external). -->
									{#if u.is_external}
										<button
											class="icon-btn icon-btn--success"
											data-testid={`admin-user-promote-${u.id}`}
											title={t('admin.promote_to_internal_title', 'Promote to internal user')}
											aria-label={t('admin.promote_to_internal_title', 'Promote to internal user')}
											onclick={() => promoteExternal(u)}
										>
											<Icon name="user-plus" />
										</button>
									{:else}
										<button
											class="icon-btn"
											data-testid={`admin-user-quota-${u.id}`}
											title={t('admin.edit_quota_title', 'Edit quota')}
											aria-label={t('admin.edit_quota_title', 'Edit quota')}
											onclick={() => openQuota(u)}
										>
											<Icon name="gauge-simple-high" />
										</button>
									{/if}
									<!-- Slot 2: reset password (local internal only —
									     OIDC and external accounts have no password
									     to reset). Placeholder otherwise. -->
									{#if !isOidcUser(u) && !u.is_external}
										<button
											class="icon-btn"
											data-testid={`admin-user-reset-password-${u.id}`}
											title={t('admin.reset_password_title', 'Reset password')}
											aria-label={t('admin.reset_password_title', 'Reset password')}
											onclick={() => openReset(u)}
										>
											<Icon name="key" />
										</button>
									{:else}
										<span class="icon-btn icon-btn--placeholder" aria-hidden="true"></span>
									{/if}
									<!-- Slot 3: role toggle. Hidden for externals — an
									     external user cannot be admin (backend guard in
									     `change_user_role` + DB CHECK
									     `users_external_not_admin`). Promotion to
									     internal is offered separately in slot 1. -->
									{#if !u.is_external}
										<button
											class="icon-btn"
											data-testid={`admin-user-toggle-role-${u.id}`}
											title={t('admin.toggle_role_title', 'Toggle admin role')}
											aria-label={t('admin.toggle_role_title', 'Toggle admin role')}
											disabled={isSelf(u)}
											onclick={() => toggleRole(u)}
										>
											<Icon name={u.role === 'admin' ? 'user' : 'crown'} />
										</button>
									{:else}
										<span class="icon-btn icon-btn--placeholder" aria-hidden="true"></span>
									{/if}
									<!-- Slot 4: activate/deactivate. -->
									<button
										class="icon-btn {u.active ? 'icon-btn--danger' : 'icon-btn--success'}"
										data-testid={`admin-user-toggle-active-${u.id}`}
										title={u.active
											? t('admin.deactivate_title', 'Deactivate')
											: t('admin.activate_title', 'Activate')}
										aria-label={u.active
											? t('admin.deactivate_title', 'Deactivate')
											: t('admin.activate_title', 'Activate')}
										disabled={isSelf(u) && u.active}
										onclick={() => toggleActive(u)}
									>
										<Icon name={u.active ? 'ban' : 'check'} />
									</button>
									<!-- Slot 5: delete. -->
									<button
										class="icon-btn icon-btn--danger"
										data-testid={`admin-user-delete-${u.id}`}
										title={t('admin.delete_title', 'Delete user')}
										aria-label={t('admin.delete_title', 'Delete user')}
										disabled={isSelf(u)}
										onclick={() => removeUser(u)}
									>
										<Icon name="trash-alt" />
									</button>
								</div>
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
			<div class="pager">
				<button
					class="btn"
					data-testid="admin-users-pager-prev-btn"
					disabled={pageIndex === 0}
					onclick={() => changePage(-1)}>‹</button
				>
				<span>{pageIndex + 1} / {Math.max(1, Math.ceil(total / PAGE_SIZE))}</span>
				<button
					class="btn"
					data-testid="admin-users-pager-next-btn"
					disabled={(pageIndex + 1) * PAGE_SIZE >= total}
					onclick={() => changePage(1)}>›</button
				>
			</div>
		{/if}
	{:else if tab === 'sessions'}
		<div class="bar">
			<label class="bar__filter">
				{t('admin.sessions.filter_user', 'User (UUID)')}
				<input
					type="text"
					placeholder="00000000-…"
					data-testid="admin-sessions-user-filter-input"
					bind:value={sessionsFilterUserId}
				/>
			</label>
			<label class="bar__toggle">
				<input
					type="checkbox"
					data-testid="admin-sessions-include-revoked-checkbox"
					bind:checked={sessionsIncludeRevoked}
				/>
				{t('admin.sessions.include_revoked', 'Include revoked / expired')}
			</label>
			<button
				class="btn"
				data-testid="admin-sessions-refresh-btn"
				onclick={() => void loadSessions()}
				disabled={sessionsLoading}
			>
				<Icon name="sync-alt" />
				{sessionsLoading ? t('common.loading', 'Loading…') : t('admin.sessions.refresh', 'Refresh')}
			</button>
		</div>
		{#if sessionsError}
			<p class="status status--error" data-testid="admin-sessions-error">{sessionsError}</p>
		{:else}
			{#if sessionsAccessTokenExpirySecs !== null}
				<!-- Revoke-lag notice: revoke breaks the refresh path
				     immediately, but a JWT already in flight stays valid
				     until its `exp` (see docs/plan/dpop.md — access tokens
				     are opaque to revocation between refreshes). Server
				     publishes the current TTL so this text is honest
				     rather than a hardcoded guess. -->
				<p class="status status--info" role="note" data-testid="admin-sessions-revoke-lag-notice">
					{t(
						'admin.sessions.revoke_lag_notice',
						{ secs: sessionsAccessTokenExpirySecs },
						'Revoking a session breaks its refresh path immediately, but any JWT already in the browser stays valid for up to {{secs}} seconds until the next refresh attempt.'
					)}
				</p>
			{/if}
			<table class="table" data-testid="admin-sessions-table">
				<thead>
					<tr>
						<th>{t('admin.sessions.col_user', 'User')}</th>
						<th>{t('admin.sessions.col_origin', 'Origin')}</th>
						<th>{t('admin.sessions.col_created', 'Created')}</th>
						<th>{t('admin.sessions.col_expires', 'Expires')}</th>
						<th>{t('admin.sessions.col_ip', 'IP')}</th>
						<th>{t('admin.sessions.col_user_agent', 'User agent')}</th>
						<th>{t('admin.sessions.col_bound', 'Bound')}</th>
						<th class="col-center">{t('admin.sessions.col_status', 'Status')}</th>
						<th></th>
					</tr>
				</thead>
				<tbody>
					{#each sessions as s (s.id)}
						<tr
							data-testid={`admin-sessions-row-${s.id}`}
							class:muted={!s.is_active}
							class:current-session={s.is_current}
						>
							<td>
								<div class="user-vignette-cell">
									<UserVignette userId={s.user_id} fallbackLabel={s.user_id} />
								</div>
							</td>
							<td data-testid={`admin-sessions-origin-${s.id}`}>
								<span class="badge badge--origin badge--origin-{s.origin}">
									{t(`admin.sessions.origin.${s.origin}`, s.origin)}
								</span>
							</td>
							<td>{new Date(s.created_at).toLocaleString()}</td>
							<td>{new Date(s.expires_at).toLocaleString()}</td>
							<td class="mono">{s.ip_address ?? '—'}</td>
							<td class="truncate" title={s.user_agent ?? ''}>
								{shortUserAgent(s.user_agent)}
							</td>
							<td>
								{#if s.is_bound}
									<span
										class="bound-cell"
										title={t(
											'admin.sessions.bound_tooltip',
											{ prefix: s.dpop_jkt_prefix ?? '' },
											'DPoP-bound (jkt {{prefix}}…)'
										)}
									>
										<Icon name="lock" />
										<span class="mono">{s.dpop_jkt_prefix ?? ''}</span>
									</span>
								{:else}
									<span class="muted">{t('admin.sessions.unbound', 'unbound')}</span>
								{/if}
							</td>
							<td class="col-center">
								{#if s.is_revoked}
									<span class="badge badge--inactive">
										{t('admin.sessions.revoked', 'revoked')}
									</span>
								{:else if !s.is_active}
									<span class="badge badge--inactive">
										{t('admin.sessions.expired', 'expired')}
									</span>
								{:else}
									<!--
										Presence dot — filled green when the server saw a
										request in the last 5 min (backend `is_online`),
										outlined grey otherwise. Only rendered on active
										rows: a revoked-but-recently-seen row would otherwise
										flash green post-revocation. Tooltip carries the
										human "last seen X ago" so admins don't have to
										hover-hunt for the exact timestamp — the
										`last_seen_at` DateTime is available as
										`title` for the details-on-demand case.
									-->
									<span
										class="presence-dot"
										class:presence-dot--online={s.is_online}
										class:presence-dot--idle={!s.is_online}
										title={s.is_online
											? t(
													'admin.sessions.presence_online_tooltip',
													{ ago: timeAgo(s.last_seen_at) },
													'Online — last seen {{ago}}'
												)
											: t(
													'admin.sessions.presence_idle_tooltip',
													{ ago: timeAgo(s.last_seen_at) },
													'Idle — last seen {{ago}}'
												)}
										aria-label={s.is_online
											? t('admin.sessions.online', 'online')
											: t('admin.sessions.idle', 'idle')}
									></span>
									<span class="badge badge--active">
										{t('admin.sessions.active', 'active')}
									</span>
								{/if}
								{#if s.is_current}
									<span
										class="badge badge--self"
										title={t(
											'admin.sessions.current_tooltip',
											"This is the session you're using right now — revoking it will log you out."
										)}
									>
										{t('admin.you_badge', 'you')}
									</span>
								{/if}
							</td>
							<td>
								{#if !s.is_revoked}
									<button
										class="icon-btn icon-btn--danger"
										data-testid={`admin-sessions-revoke-btn-${s.id}`}
										title={t('admin.sessions.revoke', 'Revoke')}
										aria-label={t('admin.sessions.revoke', 'Revoke')}
										onclick={() => void onRevokeSession(s.id, s.is_current)}
										disabled={sessionRevokingId === s.id}
									>
										<Icon name="trash-alt" />
									</button>
								{/if}
							</td>
						</tr>
					{/each}
					{#if sessions.length === 0 && !sessionsLoading}
						<tr>
							<td colspan="9" class="muted">
								{t('admin.sessions.empty', 'No sessions match the current filter.')}
							</td>
						</tr>
					{/if}
				</tbody>
			</table>
		{/if}
	{:else if tab === 'mounts'}
		<section class="admin-section" data-testid="admin-mounts-section">
			<h2>{t('admin.mounts.title', 'External File Mounts')}</h2>
			<p class="muted">
				{t(
					'admin.mounts.help',
					'Mount a host directory as a folder in your drive. Files stay on the host and are read live; deletes here are permanent.'
				)}
			</p>

			<form
				class="mount-form"
				onsubmit={(e) => {
					e.preventDefault();
					void createMount();
				}}
			>
				<input
					type="text"
					placeholder={t('admin.mounts.name', 'Name')}
					bind:value={newMount.name}
					data-testid="mount-name"
				/>
				<input
					type="text"
					placeholder={t('admin.mounts.path', 'Host path (e.g. /srv/media)')}
					bind:value={newMount.host_path}
					data-testid="mount-path"
				/>
				<label>
					<input type="checkbox" bind:checked={newMount.read_only} />
					{t('admin.mounts.readonly', 'Read-only')}
				</label>
				<button type="submit" disabled={mountCreating} data-testid="mount-create">
					{t('admin.mounts.add', 'Add mount')}
				</button>
			</form>

			{#if mountsError}
				<p class="error" data-testid="mount-error">{mountsError}</p>
			{/if}

			{#if mounts}
				{#if mounts.length === 0}
					<p class="muted">{t('admin.mounts.empty', 'No mounts configured.')}</p>
				{:else}
					<table class="table">
						<thead>
							<tr>
								<th>{t('admin.mounts.name', 'Name')}</th>
								<th>{t('admin.mounts.kind', 'Kind')}</th>
								<th>{t('admin.mounts.path', 'Path')}</th>
								<th>{t('admin.mounts.readonly', 'Read-only')}</th>
								<th></th>
							</tr>
						</thead>
						<tbody>
							{#each mounts as m (m.mount_folder_id)}
								<tr>
									<td>{m.name}</td>
									<td>{m.kind}</td>
									<td class="muted">{m.mount_path}</td>
									<td>{m.read_only ? t('common.yes', 'Yes') : t('common.no', 'No')}</td>
									<td>
										<button
											class="danger"
											onclick={() => void deleteMount(m.mount_folder_id)}
											data-testid="mount-delete"
										>
											{t('common.delete', 'Delete')}
										</button>
									</td>
								</tr>
							{/each}
						</tbody>
					</table>
				{/if}
			{:else}
				<p class="muted">{t('common.loading', 'Loading…')}</p>
			{/if}
		</section>
	{:else if tab === 'drives'}
		<div class="bar">
			<button
				class="btn btn--primary"
				data-testid="admin-drives-create-btn"
				onclick={openDriveCreate}
			>
				<Icon name="plus" />
				{t('admin.create_drive', 'Create shared drive')}
			</button>
		</div>
		{#if drivesError}
			<p class="status status--error">{drivesError}</p>
		{:else if drivesList.length === 0}
			<p class="status">{t('admin.no_drives', 'No drives yet.')}</p>
		{:else}
			<table class="table">
				<thead>
					<tr>
						<th>{t('admin.drive_name', 'Name')}</th>
						<th>{t('admin.drive_kind', 'Kind')}</th>
						<th>{t('admin.drive_owners', 'Owners')}</th>
						<th>{t('admin.drive_usage', 'Usage')}</th>
						<th>{t('admin.drive_created_at', 'Created')}</th>
						<th></th>
					</tr>
				</thead>
				<tbody>
					{#each drivesList as d (d.id)}
						{@const owner = d.kind === 'personal' ? personalDriveOwners[d.id] : undefined}
						<!--
						  Effective quota:
						  - Shared drive: `d.quota_bytes` (null → unlimited, shown as ∞).
						  - Personal drive: the owner user's `storage_quota_bytes`
						    envelope caps the sum of used_bytes across their personal
						    drives (per memory `project_user_envelope_quota_model`).
						    `d.quota_bytes` is always null for personal drives so we
						    fall back to the owner's cap; 0 also means "no limit"
						    (backend convention — see `User.storage_quota_bytes` doc).
						-->
						{@const effectiveQuota =
							d.kind === 'personal'
								? owner && owner.storage_quota_bytes > 0
									? owner.storage_quota_bytes
									: null
								: d.quota_bytes && d.quota_bytes > 0
									? d.quota_bytes
									: null}
						{@const pct =
							effectiveQuota !== null ? Math.min(100, (d.used_bytes / effectiveQuota) * 100) : null}
						<tr>
							<td>
								<div class="user-cell">
									<strong>{d.name}</strong>
									{#if d.kind === 'personal'}
										{#if owner}
											<span class="muted">{owner.username ?? owner.email}</span>
										{:else}
											<span class="muted">{t('common.loading', 'Loading…')}</span>
										{/if}
									{/if}
								</div>
							</td>
							<td>
								<div class="drive-kind-cell">
									<span class="badge badge--{d.kind === 'shared' ? 'admin' : 'user'}">
										{driveKindLabel(d)}
									</span>
									{#if driveDefaultSuffix(d)}
										<span class="muted drive-kind-cell__suffix">{driveDefaultSuffix(d)}</span>
									{/if}
								</div>
							</td>
							<td>
								{#if driveMembers[d.id]}
									<OwnerAvatarStack members={driveMembers[d.id]} />
								{:else}
									<span class="muted">{t('common.loading', 'Loading…')}</span>
								{/if}
							</td>
							<td>
								<div class="quota-cell">
									{#if pct !== null}
										<div class="quota-bar">
											<div
												class="quota-fill"
												class:quota-fill--warn={pct > 70}
												class:quota-fill--danger={pct > 90}
												style:width="{pct}%"
											></div>
										</div>
									{/if}
									<span class="muted">
										{formatBytes(d.used_bytes)} / {effectiveQuota !== null
											? formatBytes(effectiveQuota)
											: '∞'}
									</span>
								</div>
							</td>
							<td class="muted">{timeAgo(d.created_at)}</td>
							<td>
								<!-- Wrapper div carries the `actions` flex layout; the
								     <td> stays a plain table cell so its baseline +
								     bottom-border align with the rest of the row even on
								     personal-drive rows where the wrapper is empty. -->
								<div class="actions actions--drive">
									<!-- Each action sits in a fixed grid column so icons
									     line up across rows even when the row's drive
									     kind doesn't support some of them (personal drives
									     have no owner roster; default drives can't be
									     deleted). Inapplicable actions render as invisible
									     placeholders to reserve their column. -->
									{#if d.kind === 'shared'}
										<button
											class="icon-btn"
											data-testid={`admin-drive-manage-owners-${d.id}`}
											title={t('admin.drive_manage_owners', 'Manage owners')}
											aria-label={t('admin.drive_manage_owners', 'Manage owners')}
											onclick={() => openManageOwners(d)}
										>
											<Icon name="users-cog" />
										</button>
									{:else}
										<span class="icon-btn icon-btn--placeholder" aria-hidden="true"></span>
									{/if}
									<!-- D5 policy editor — admin-only mutation (the
									     owner UI no longer surfaces policies at all).
									     Available on every drive kind including personal,
									     so the operator can lock a personal drive's
									     external-sharing surface from outside. -->
									<button
										class="icon-btn"
										data-testid={`admin-drive-manage-policies-${d.id}`}
										title={t('admin.drive_manage_policies', 'Manage policies')}
										aria-label={t('admin.drive_manage_policies', 'Manage policies')}
										onclick={() => openManagePolicies(d)}
									>
										<Icon name="shield-alt" />
									</button>
									<!-- Shared-drive quota edit. Personal drives use the
									     owner user's `storage_quota_bytes` envelope; the
									     backend refuses PATCH /api/drives/{id}/quota on a
									     personal drive with 400 (see the service-layer
									     guard and Step 25 of drive_quota.hurl). Render an
									     invisible placeholder on personal rows so the
									     column stays aligned. -->
									{#if d.kind === 'shared'}
										<button
											class="icon-btn"
											data-testid={`admin-drive-edit-quota-${d.id}`}
											title={t('admin.drive_edit_quota', 'Edit quota')}
											aria-label={t('admin.drive_edit_quota', 'Edit quota')}
											onclick={() => openDriveQuota(d)}
										>
											<Icon name="gauge-simple-high" />
										</button>
									{:else}
										<span class="icon-btn icon-btn--placeholder" aria-hidden="true"></span>
									{/if}
									<!-- Default-personal drives can never be deleted
									     (backend returns 405). Render an invisible
									     placeholder so the row's columns still line up
									     with the deletable rows above and below. -->
									{#if !d.default_for_user}
										<button
											class="icon-btn icon-btn--danger"
											data-testid={`admin-drive-delete-${d.id}`}
											title={t('admin.drive_delete', 'Delete drive')}
											aria-label={t('admin.drive_delete', 'Delete drive')}
											onclick={() => requestDeleteDrive(d)}
										>
											<Icon name="trash-alt" />
										</button>
									{:else}
										<span class="icon-btn icon-btn--placeholder" aria-hidden="true"></span>
									{/if}
								</div>
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		{/if}
	{:else if tab === 'jobs'}
		<AdminJobsPanel />
	{:else if !pluginsAvailable}
		<p class="status">{t('admin.plugins_disabled', 'The plugin subsystem is disabled.')}</p>
	{:else if pluginsError}
		<p class="status status--error">{pluginsError}</p>
	{:else}
		<div class="install-bar">
			<div>
				<strong>{t('admin.plugins_install', 'Install plugin')}</strong>
				<span class="muted"
					>{t('admin.plugins_install_hint', 'Upload a plugin bundle (.zip).')}</span
				>
			</div>
			<label class="btn btn-primary" class:disabled={installing}>
				<Icon name="cloud-upload-alt" />
				{installing
					? t('admin.plugins_installing', 'Installing…')
					: t('admin.plugins_upload', 'Upload .zip')}
				<input
					type="file"
					data-testid="admin-plugins-install-input"
					accept=".zip,application/zip"
					hidden
					disabled={installing}
					onchange={onInstallPlugin}
				/>
			</label>
		</div>
		{#if installMsg}
			<p class={installMsg.ok ? 'status--ok' : 'status--error'}>{installMsg.text}</p>
		{/if}
		{#if plugins.length === 0}
			<p class="status">{t('admin.no_plugins', 'No plugins installed.')}</p>
		{:else}
			<table class="table">
				<thead>
					<tr>
						<th>{t('admin.plugin', 'Plugin')}</th>
						<th>{t('admin.plugins_col_id', 'ID')}</th>
						<th>{t('admin.version', 'Version')}</th>
						<th>{t('admin.plugins_col_events', 'Events')}</th>
						<th>{t('admin.status', 'Status')}</th>
						<th></th>
					</tr>
				</thead>
				<tbody>
					{#each plugins as p (p.id)}
						<tr>
							<td>
								<div class="user-cell">
									<strong>{p.name}</strong>
									{#if p.description}<span class="muted">{p.description}</span>{/if}
								</div>
							</td>
							<td><code>{p.id}</code></td>
							<td>{p.version ?? '—'}</td>
							<td>
								{#if p.subscriptions && p.subscriptions.length > 0}
									<span class="events">{p.subscriptions.length}</span>
								{:else}
									—
								{/if}
							</td>
							<td>
								<span class="badge badge--{p.enabled ? 'active' : 'inactive'}">
									{p.enabled ? t('admin.enabled', 'Enabled') : t('admin.disabled', 'Disabled')}
								</span>
							</td>
							<td class="actions">
								<button
									class="icon-btn"
									data-testid={`admin-plugin-details-${p.id}`}
									title={t('admin.plugins_details', 'Logs & details')}
									aria-label={t('admin.plugins_details', 'Logs & details')}
									onclick={() => openLogs(p)}
								>
									<Icon name="list" />
								</button>
								<button
									class="icon-btn {p.enabled ? '' : 'icon-btn--success'}"
									data-testid={`admin-plugin-toggle-${p.id}`}
									title={p.enabled ? t('admin.disable', 'Disable') : t('admin.enable', 'Enable')}
									aria-label={p.enabled
										? t('admin.disable', 'Disable')
										: t('admin.enable', 'Enable')}
									onclick={() => togglePlugin(p)}
								>
									<Icon name={p.enabled ? 'pause' : 'play'} />
								</button>
								<button
									class="icon-btn icon-btn--danger"
									data-testid={`admin-plugin-delete-${p.id}`}
									title={t('common.delete', 'Delete')}
									aria-label={t('common.delete', 'Delete')}
									onclick={() => removePlugin(p)}
								>
									<Icon name="trash-alt" />
								</button>
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		{/if}
	{/if}
</main>

<Modal bind:open={createOpen} title={t('admin.create_user', 'Create user')}>
	<form
		id="create-user-form"
		data-testid="admin-create-user-form"
		onsubmit={submitCreate}
		class="form"
	>
		<label
			><span>{t('admin.username', 'Username')}</span>
			<input
				bind:value={newUser.username}
				data-testid="admin-create-user-username-input"
				minlength="3"
				required
			/></label
		>
		<label
			><span
				>{t('admin.email', 'Email')}
				<span class="muted">({t('common.optional', 'optional')})</span></span
			>
			<input
				type="email"
				data-testid="admin-create-user-email-input"
				bind:value={newUser.email}
				placeholder={t('admin.email_auto', 'Auto-generated if left blank')}
			/></label
		>
		<label
			><span>{t('admin.password', 'Password')}</span>
			<input
				type="password"
				data-testid="admin-create-user-password-input"
				bind:value={newUser.password}
				minlength="8"
				required
			/></label
		>
		<label
			><span>{t('admin.role', 'Role')}</span>
			<select bind:value={newUser.role} data-testid="admin-create-user-role-select">
				<option value="user">user</option>
				<option value="admin">admin</option>
			</select></label
		>
		<label
			><span>{t('admin.quota', 'Quota')}</span>
			<div class="quota-input">
				<input
					type="number"
					data-testid="admin-create-user-quota-input"
					min="0"
					step="0.1"
					bind:value={newUser.quotaValue}
				/>
				<select bind:value={newUser.quotaUnit} data-testid="admin-create-user-quota-unit-select">
					{#each QUOTA_UNITS as unit (unit.label)}<option value={unit.value}>{unit.label}</option
						>{/each}
				</select>
			</div>
			<span class="muted">{t('admin.quota_unlimited_hint', '0 = unlimited')}</span></label
		>
		{#if createError}<p class="status--error">{createError}</p>{/if}
	</form>
	{#snippet footer()}
		<button
			class="btn"
			data-testid="admin-create-user-cancel-btn"
			onclick={() => (createOpen = false)}>{t('common.cancel', 'Cancel')}</button
		>
		<button
			class="btn btn--primary"
			type="submit"
			form="create-user-form"
			data-testid="admin-create-user-submit-btn"
			disabled={creating}
		>
			{creating ? t('admin.creating', 'Creating…') : t('common.create', 'Create')}
		</button>
	{/snippet}
</Modal>

<!-- Create-drive modal (D3a). Personal-drive creation is omitted because
     the backend returns 501 for kind=personal today; see DrivePicker for
     UI flow and drive_handler::create_drive for the wire contract. -->
<Modal
	open={driveCreateOpen}
	title={t('admin.create_drive', 'Create shared drive')}
	onclose={() => (driveCreateOpen = false)}
>
	<form
		id="create-drive-form"
		class="form"
		data-testid="admin-create-drive-form"
		onsubmit={submitDriveCreate}
	>
		<label>
			<span>{t('admin.drive_name', 'Name')}</span>
			<input
				bind:value={driveForm.name}
				data-testid="admin-create-drive-name-input"
				required
				placeholder={t('admin.drive_name_placeholder', 'e.g. Engineering')}
			/>
		</label>
		<label class="drive-owner">
			<span>{t('admin.drive_owner', 'Owner')}</span>
			<input
				type="text"
				data-testid="admin-create-drive-owner-input"
				bind:value={driveForm.ownerQuery}
				oninput={(e) => searchOwnerCandidates(e.currentTarget.value)}
				placeholder={t('admin.drive_owner_placeholder', 'Search a user or group…')}
				autocomplete="off"
				required
			/>
			{#if ownerSearching}
				<span class="muted">{t('common.loading', 'Loading…')}</span>
			{:else if ownerSuggestions.length > 0}
				<ul class="owner-suggest" role="listbox">
					{#each ownerSuggestions as r (`${r.type}-${r.id}`)}
						<li>
							<button
								type="button"
								class="owner-suggest__row"
								data-testid={`admin-drive-owner-pick-${r.type}-${r.id}`}
								onclick={() => pickOwner(r)}
							>
								<Icon name={r.type === 'group' ? 'users' : 'user'} />
								<span class="owner-suggest__label">{r.label}</span>
								{#if r.sublabel}
									<span class="muted">{r.sublabel}</span>
								{/if}
							</button>
						</li>
					{/each}
				</ul>
			{/if}
			{#if driveForm.ownerPick}
				<span class="muted owner-pick">
					<Icon name={driveForm.ownerPick.type === 'group' ? 'users' : 'user'} />
					{t('admin.drive_owner_picked', { name: driveForm.ownerPick.label }, 'Owner: {{name}}')}
				</span>
			{/if}
			<span class="muted">
				{t(
					'admin.drive_owner_hint',
					'Pick a user (sole Owner) or a group (every member becomes Owner via subject expansion).'
				)}
			</span>
		</label>
		<label>
			<span>{t('admin.quota', 'Quota')}</span>
			<div class="quota-input">
				<input
					type="number"
					data-testid="admin-create-drive-quota-input"
					min="0"
					step="0.1"
					bind:value={driveForm.quotaValue}
				/>
				<select bind:value={driveForm.quotaUnit} data-testid="admin-create-drive-quota-unit-select">
					{#each QUOTA_UNITS as unit (unit.label)}
						<option value={unit.value}>{unit.label}</option>
					{/each}
				</select>
			</div>
			<span class="muted">{t('admin.quota_unlimited_hint', '0 = unlimited')}</span>
		</label>
		{#if driveCreateError}<p class="status--error">{driveCreateError}</p>{/if}
	</form>
	{#snippet footer()}
		<button
			class="btn"
			data-testid="admin-create-drive-cancel-btn"
			onclick={() => (driveCreateOpen = false)}
		>
			{t('common.cancel', 'Cancel')}
		</button>
		<button
			class="btn btn--primary"
			type="submit"
			form="create-drive-form"
			data-testid="admin-create-drive-submit-btn"
			disabled={driveCreating}
		>
			{driveCreating ? t('admin.creating', 'Creating…') : t('common.create', 'Create')}
		</button>
	{/snippet}
</Modal>

<!-- Manage-owners modal (D3a admin bypass — calls
     /api/admin/drives/{id}/members POST/DELETE which skip the per-drive
     `Manage` check). Last-owner protection still applies server-side. -->
<Modal
	open={manageOwnersDrive !== null}
	title={manageOwnersDrive
		? t(
				'admin.drive_manage_owners_for',
				{ name: manageOwnersDrive.name },
				'Manage owners — {{name}}'
			)
		: t('admin.drive_manage_owners', 'Manage owners')}
	onclose={closeManageOwners}
>
	{#if manageOwnersDrive}
		<div class="form">
			<div>
				<label for="manage-owners-search">
					<span>{t('admin.drive_add_owner', 'Add owner')}</span>
				</label>
				<input
					id="manage-owners-search"
					type="text"
					data-testid="admin-manage-owners-search-input"
					bind:value={manageOwnersQuery}
					oninput={(e) => searchManageOwnersCandidates(e.currentTarget.value)}
					placeholder={t('admin.drive_owner_placeholder', 'Search a user or group…')}
					autocomplete="off"
					disabled={manageOwnersBusy}
				/>
				{#if manageOwnersSearching}
					<span class="muted">{t('common.loading', 'Loading…')}</span>
				{:else if manageOwnersSuggestions.length > 0}
					<ul class="owner-suggest" role="listbox">
						{#each manageOwnersSuggestions as r (`${r.type}-${r.id}`)}
							<li>
								<button
									type="button"
									class="owner-suggest__row"
									data-testid={`admin-manage-owners-pick-${r.type}-${r.id}`}
									onclick={() => addOwner(r)}
									disabled={manageOwnersBusy}
								>
									<Icon name={r.type === 'group' ? 'users' : 'user'} />
									<span class="owner-suggest__label">{r.label}</span>
									{#if r.sublabel}<span class="muted">{r.sublabel}</span>{/if}
								</button>
							</li>
						{/each}
					</ul>
				{/if}
			</div>

			<div>
				<h3 class="owners-list__title">
					{t('admin.drive_current_owners', 'Current owners')}
					<span class="muted">({manageOwnersList.length})</span>
				</h3>
				{#if manageOwnersList.length === 0}
					<p class="muted">{t('admin.drive_no_owners', 'No owners')}</p>
				{:else}
					<ul class="owners-list">
						{#each manageOwnersList as m (`${m.subject.type}-${m.subject.id}`)}
							<li class="owners-list__row">
								{#if m.subject.type === 'user'}
									<UserVignette userId={m.subject.id} />
								{:else}
									<!-- Groups don't resolve via /api/users/{id}; render an
									     inline equivalent using the cached recipient label
									     from the share-search resolver. -->
									<span class="owners-list__group">
										<span class="owners-list__group-icon"><Icon name="users" /></span>
										<span class="owners-list__group-name">
											{resolveRecipient('group', m.subject.id).label}
										</span>
									</span>
								{/if}
								<button
									type="button"
									class="icon-btn icon-btn--danger"
									data-testid={`admin-manage-owners-remove-${m.subject.type}-${m.subject.id}`}
									title={t('common.remove', 'Remove')}
									aria-label={t('common.remove', 'Remove')}
									onclick={() => removeOwner(m)}
									disabled={manageOwnersBusy}
								>
									<Icon name="trash-alt" />
								</button>
							</li>
						{/each}
					</ul>
				{/if}
			</div>

			{#if manageOwnersError}
				<p class="status--error">{manageOwnersError}</p>
			{/if}
		</div>
	{/if}
	{#snippet footer()}
		<button class="btn" data-testid="admin-manage-owners-close-btn" onclick={closeManageOwners}>
			{t('common.close', 'Close')}
		</button>
	{/snippet}
</Modal>

<!-- Manage-policies modal (D5 admin-only). Toggles for the five known
     policy keys; unknown keys on the JSONB bag are preserved by the
     backend merge but not surfaced here (forward-compat is at the
     server). Save → PATCH /api/drives/{id}/policies. -->
<Modal
	open={managePoliciesDrive !== null}
	title={managePoliciesDrive
		? t(
				'admin.drive_manage_policies_for',
				{ name: managePoliciesDrive.name },
				'Manage policies — {{name}}'
			)
		: t('admin.drive_manage_policies', 'Manage policies')}
	onclose={closeManagePolicies}
>
	{#if managePoliciesDrive}
		<div class="form">
			<p class="muted">
				{t(
					'admin.drive_manage_policies_help',
					'Policies are admin-only — drive owners cannot mutate them. Each toggle controls one enforcement gate.'
				)}
			</p>
			<PolicyList
				values={managePoliciesDraft}
				busy={managePoliciesBusy}
				testIdPrefix="admin-policy"
				onchange={(key, next) => {
					managePoliciesDraft[key] = next;
				}}
			/>
			{#if managePoliciesError}
				<p class="status--error">{managePoliciesError}</p>
			{/if}
		</div>
	{/if}
	{#snippet footer()}
		<button
			class="btn"
			data-testid="admin-manage-policies-cancel-btn"
			onclick={closeManagePolicies}
			disabled={managePoliciesBusy}
		>
			{t('common.cancel', 'Cancel')}
		</button>
		<button
			class="btn btn-primary"
			data-testid="admin-manage-policies-save-btn"
			onclick={saveManagePolicies}
			disabled={managePoliciesBusy}
		>
			{managePoliciesBusy ? t('common.saving', 'Saving…') : t('common.save', 'Save')}
		</button>
	{/snippet}
</Modal>

<!-- User-envelope quota edit — uses the shared <QuotaEditor>.
     "Unlimited" checkbox maps to 0 on the wire; positive value * unit
     is sent verbatim to `setUserQuota`. -->
<QuotaEditor
	open={quotaModal !== null}
	title={t('admin.edit_quota_title', 'Edit quota')}
	subjectName={quotaModal?.username ?? ''}
	initialBytes={quotaModal?.initialBytes ?? 0}
	busy={quotaModalBusy}
	error={quotaModalError}
	testIdPrefix="admin-user-quota"
	onclose={() => (quotaModal = null)}
	onsave={saveQuota}
/>

<!-- Shared-drive quota edit — same component, different endpoint.
     "Unlimited" maps to `null` on the wire (see saveDriveQuota). -->
<QuotaEditor
	open={driveQuotaModal !== null}
	title={t('admin.drive_edit_quota', 'Edit quota')}
	subjectName={driveQuotaModal?.driveName ?? ''}
	initialBytes={driveQuotaModal?.initialBytes ?? null}
	busy={driveQuotaBusy}
	error={driveQuotaError}
	testIdPrefix="admin-drive-quota"
	onclose={() => (driveQuotaModal = null)}
	onsave={saveDriveQuota}
/>

<!-- Reset-password modal -->
<Modal
	open={resetModal !== null}
	title={t('admin.reset_password_title', 'Reset password')}
	onclose={() => (resetModal = null)}
>
	{#if resetModal}
		<form
			id="reset-pw-form"
			class="form"
			data-testid="admin-reset-password-form"
			onsubmit={submitReset}
		>
			<p class="muted">
				{t('admin.reset_pw_for', 'New password for')} <strong>{resetModal.username}</strong>
			</p>
			<label
				><span>{t('admin.new_password', 'New password')}</span>
				<input
					type="password"
					data-testid="admin-reset-password-input"
					bind:value={resetPassword}
					minlength="8"
					required
				/></label
			>
			{#if resetError}<p class="status--error">{resetError}</p>{/if}
		</form>
	{/if}
	{#snippet footer()}
		<button
			class="btn"
			data-testid="admin-reset-password-cancel-btn"
			onclick={() => (resetModal = null)}>{t('common.cancel', 'Cancel')}</button
		>
		<button
			class="btn btn--primary"
			type="submit"
			form="reset-pw-form"
			data-testid="admin-reset-password-submit-btn"
			disabled={resetting}
		>
			{resetting ? t('admin.resetting', 'Resetting…') : t('admin.reset_btn', 'Reset')}
		</button>
	{/snippet}
</Modal>

<!-- Delete-user confirmation modal — typed-email gate. The Delete
     button stays disabled until the admin re-types the target's
     email address, matching case-insensitively. Extra friction on a
     destructive, irreversible action. -->
<Modal
	open={deleteUserModal !== null}
	title={t('admin.delete_user_title', 'Delete user')}
	onclose={() => (deleteUserModal = null)}
>
	{#if deleteUserModal}
		<form
			id="delete-user-form"
			class="form"
			data-testid="admin-delete-user-form"
			onsubmit={(e) => {
				e.preventDefault();
				void confirmDeleteUser();
			}}
		>
			<p>
				{t(
					'admin.delete_user_warning',
					{ name: deleteUserModal.username },
					'You are about to permanently delete "{{name}}". This will remove the account, revoke every session, and reap the personal drive. This cannot be undone.'
				)}
			</p>
			<label>
				<span>
					{t(
						'admin.delete_user_confirm_hint',
						{ email: deleteUserModal.email },
						'To confirm, type the account email below: {{email}}'
					)}
				</span>
				<input
					type="email"
					data-testid="admin-delete-user-email-input"
					autocomplete="off"
					bind:value={deleteUserEmailInput}
					placeholder={deleteUserModal.email}
					disabled={deleteUserBusy}
					required
				/>
			</label>
		</form>
	{/if}
	{#snippet footer()}
		<button
			class="btn"
			data-testid="admin-delete-user-cancel-btn"
			onclick={() => (deleteUserModal = null)}
			disabled={deleteUserBusy}
		>
			{t('common.cancel', 'Cancel')}
		</button>
		<button
			class="btn btn--danger"
			type="submit"
			form="delete-user-form"
			data-testid="admin-delete-user-confirm-btn"
			disabled={!deleteUserEmailMatches || deleteUserBusy}
		>
			{deleteUserBusy ? t('admin.deleting', 'Deleting…') : t('admin.delete_title', 'Delete user')}
		</button>
	{/snippet}
</Modal>

<!-- Styled confirm modal (replaces native confirm) -->
<Modal
	open={confirmState !== null}
	title={t('common.confirm', 'Confirm')}
	onclose={() => resolveConfirm(false)}
>
	<p>{confirmState?.message}</p>
	{#snippet footer()}
		<button class="btn" data-testid="admin-confirm-cancel-btn" onclick={() => resolveConfirm(false)}
			>{t('common.cancel', 'Cancel')}</button
		>
		<button
			class="btn btn--primary"
			data-testid="admin-confirm-ok-btn"
			onclick={() => resolveConfirm(true)}
		>
			{t('common.confirm', 'Confirm')}
		</button>
	{/snippet}
</Modal>

<Modal
	open={logsPlugin !== null}
	title={logsPlugin?.name ?? t('admin.plugin_logs', 'Plugin logs')}
	onclose={closeLogs}
>
	{#if logsPlugin}
		<dl class="kv plugin-meta">
			<dt>{t('admin.plugins_col_id', 'ID')}</dt>
			<dd><code>{logsPlugin.id}</code></dd>
			<dt>{t('admin.version', 'Version')}</dt>
			<dd>{logsPlugin.version ?? '—'}</dd>
			{#if logsPlugin.abi != null}
				<dt>ABI</dt>
				<dd>{logsPlugin.abi}</dd>
			{/if}
			<dt>{t('admin.plugins_col_events', 'Events')}</dt>
			<dd>
				{#if logsPlugin.subscriptions && logsPlugin.subscriptions.length > 0}
					{#each logsPlugin.subscriptions as ev (ev)}<code class="event-tag">{ev}</code>
					{/each}
				{:else}
					—
				{/if}
			</dd>
			<dt>{t('admin.status', 'Status')}</dt>
			<dd>
				<span class="badge badge--{logsPlugin.enabled ? 'active' : 'inactive'}">
					{logsPlugin.enabled ? t('admin.enabled', 'Enabled') : t('admin.disabled', 'Disabled')}
				</span>
			</dd>
		</dl>
	{/if}

	{#if retention}
		<form
			class="form retention-form"
			data-testid="admin-plugin-retention-form"
			onsubmit={(e) => (e.preventDefault(), saveRetention())}
		>
			<h3>{t('admin.plugins_retention', 'Log retention')}</h3>
			<label
				><span>{t('admin.plugins_retention_days', 'Keep for (days)')}</span>
				<input
					type="number"
					data-testid="admin-plugin-retention-days-input"
					min="0"
					bind:value={retentionDays}
				/></label
			>
			<label
				><span>{t('admin.plugins_retention_max', 'Max size (MB)')}</span>
				<input
					type="number"
					data-testid="admin-plugin-retention-max-input"
					min="0"
					bind:value={retentionMb}
				/></label
			>
			{#if retentionMsg}<p class="muted">{retentionMsg}</p>{/if}
			<button class="btn btn-secondary" type="submit" data-testid="admin-plugin-retention-save-btn"
				>{t('admin.plugins_retention_save', 'Save retention')}</button
			>
		</form>
	{/if}

	<div class="logs-toolbar">
		<select
			bind:value={logsLevel}
			data-testid="admin-plugin-logs-level-select"
			onchange={reloadLogsFromStart}
		>
			<option value="">{t('admin.logs_all', 'All levels')}</option>
			<option value="info">info</option>
			<option value="warn">warn</option>
			<option value="error">error</option>
		</select>
		<input
			placeholder={t('admin.logs_search', 'Search…')}
			data-testid="admin-plugin-logs-search-input"
			bind:value={logsSearch}
			onkeydown={(e) => e.key === 'Enter' && reloadLogsFromStart()}
		/>
		<button
			class="btn btn-secondary"
			data-testid="admin-plugin-logs-search-btn"
			onclick={reloadLogsFromStart}>{t('common.search', 'Search')}</button
		>
		<label class="live-toggle">
			<input
				type="checkbox"
				data-testid="admin-plugin-logs-live-checkbox"
				bind:checked={logsLive}
				onchange={toggleLive}
			/>
			<span>{t('admin.logs_live', 'Live')}</span>
		</label>
	</div>
	{#if logsLoading}
		<p class="status">{t('common.loading', 'Loading…')}</p>
	{:else if logs.length === 0}
		<p class="status">{t('admin.logs_empty', 'No log entries.')}</p>
	{:else}
		<div class="logs-table-wrap">
			<table class="table logs-table">
				<thead>
					<tr>
						<th>{t('admin.logs_time', 'Time')}</th>
						<th>{t('admin.logs_level', 'Level')}</th>
						<th>{t('admin.logs_kind', 'Kind')}</th>
						<th>{t('admin.logs_invocation', 'Invocation')}</th>
						<th>{t('admin.logs_message', 'Message')}</th>
					</tr>
				</thead>
				<tbody>
					{#each logs as entry, i (i)}
						<tr class="log-row log-row--{(entry.level ?? 'info').toLowerCase()}">
							<td class="log-time">{timeAgo(entry.ts ?? entry.timestamp)}</td>
							<td>
								<span class="log-level log-level--{(entry.level ?? 'info').toLowerCase()}"
									>{entry.level ?? 'info'}</span
								>
							</td>
							<td><code>{logKind(entry)}</code></td>
							<td><code class="log-inv">{entry.invocation_id ?? '—'}</code></td>
							<td class="log-msg">{logMsg(entry)}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
	<div class="pager logs-pager">
		<button
			class="btn"
			data-testid="admin-plugin-logs-pager-prev-btn"
			disabled={logsPage === 0}
			onclick={logsPrev}>‹</button
		>
		<span>
			{#if logsTotal === 0}
				{t('admin.logs_empty', 'No log entries.')}
			{:else}
				{t(
					'admin.logs_showing',
					{
						from: logsPage * LOGS_PAGE_SIZE + 1,
						to: Math.min((logsPage + 1) * LOGS_PAGE_SIZE, logsTotal),
						total: logsTotal
					},
					'Showing {{from}}–{{to}} of {{total}}'
				)}
			{/if}
		</span>
		<button
			class="btn"
			data-testid="admin-plugin-logs-pager-next-btn"
			disabled={(logsPage + 1) * LOGS_PAGE_SIZE >= logsTotal}
			onclick={logsNext}>›</button
		>
	</div>
	{#snippet footer()}
		<button class="btn btn-danger" data-testid="admin-plugin-logs-clear-btn" onclick={purgeLogs}
			>{t('admin.plugins_clear_logs', 'Clear logs')}</button
		>
		<button class="btn btn-secondary" data-testid="admin-plugin-logs-close-btn" onclick={closeLogs}>
			{t('common.close', 'Close')}
		</button>
	{/snippet}
</Modal>

<style>
	/* Admin sessions panel — accent the caller's own row so revoking
	   it can't happen by muscle memory. Left-border stripe matches how
	   Users' table calls out the caller via the `you` badge; JS
	   confirms with an escalated message on top of the visual cue. */
	.current-session td:first-child {
		border-left: 3px solid var(--color-accent);
	}

	/* Center-align the sessions table's Status column (badge cluster
	   reads better centered than left-flushed under a `Status` label,
	   especially now that the `you` badge sits alongside the status
	   pill). Scoped to this component by Svelte's default style
	   isolation. */
	.col-center {
		text-align: center;
	}

	.logs-toolbar {
		display: flex;
		gap: var(--space-2);
		margin-bottom: var(--space-3);
	}

	.logs-toolbar input {
		flex: 1;
		padding: var(--space-2) var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
	}

	.live-toggle {
		display: flex;
		align-items: center;
		gap: var(--space-1);
		white-space: nowrap;
		font-size: var(--text-sm);
		color: var(--color-text-muted);
	}

	.logs-table-wrap {
		max-height: 50vh;
		overflow: auto;
	}

	.logs-table {
		font-family: var(--font-mono, monospace);
		font-size: var(--text-sm);
	}

	.log-time {
		color: var(--color-text-muted);
		white-space: nowrap;
	}

	.log-inv {
		font-size: var(--text-xs, 0.7rem);
		color: var(--color-text-muted);
	}

	.log-level {
		text-transform: uppercase;
		font-size: var(--text-xs, 0.7rem);
		font-weight: var(--weight-semibold, 600);
	}

	.log-level--error {
		color: var(--color-error-text);
	}

	.log-level--warn {
		color: var(--color-warning-text);
	}

	.log-msg {
		overflow-wrap: break-word;
	}

	.logs-pager {
		margin-top: var(--space-3);
	}

	.card {
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
		padding: var(--space-5);
		margin-bottom: var(--space-4);
	}

	.card h2 {
		margin: 0 0 var(--space-3);
		font-size: 1.125rem;
	}

	.checkbox {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	.ds-grid {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(8rem, 1fr));
		gap: var(--space-3);
		margin-bottom: var(--space-4);
	}

	.ds-card {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		padding: var(--space-4);
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
		color: var(--color-text-muted);
		font-size: var(--text-sm);
	}

	.ds-num {
		font-size: 1.5rem;
		font-weight: var(--weight-bold);
		color: var(--color-text-heading);
	}

	.ds-bar {
		height: 8px;
		background: var(--color-bg-muted);
		border-radius: var(--radius-full);
		overflow: hidden;
		margin-bottom: var(--space-2);
	}

	.ds-fill {
		height: 100%;
		background: var(--color-success-text);
	}

	.ds-fill--warn {
		background: var(--color-warning-text);
	}

	.ds-fill--danger {
		background: var(--color-error-text);
	}

	.storage-cards {
		display: grid;
		grid-template-columns: 2fr 1fr;
		gap: var(--space-3);
		margin-bottom: var(--space-4);
	}

	@media (width <= 40rem) {
		.storage-cards {
			grid-template-columns: 1fr;
		}
	}

	.storage-cards__hint {
		margin-top: calc(-1 * var(--space-2));
		margin-bottom: var(--space-3);
		font-size: var(--text-xs);
	}

	.storage-cards__stats {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		margin: 0;
	}

	.storage-cards__stats > div {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: var(--space-3);
	}

	.storage-cards__stats dt {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		color: var(--color-text-muted);
		font-size: var(--text-sm);
	}

	.storage-cards__stats dd {
		margin: 0;
		font-weight: var(--weight-semibold);
		color: var(--color-text-heading);
		font-variant-numeric: tabular-nums;
	}

	.storage-cards__stat-hint {
		cursor: help;
	}

	.quota-table {
		width: 100%;
		border-collapse: collapse;
	}

	.quota-table th,
	.quota-table td {
		padding: var(--space-2) var(--space-2);
		text-align: left;
		vertical-align: middle;
		font-size: var(--text-sm);
	}

	.quota-table th {
		font-weight: var(--weight-semibold);
		color: var(--color-text-heading);
		white-space: nowrap;
	}

	.quota-table__num {
		font-variant-numeric: tabular-nums;
		white-space: nowrap;
	}

	.quota-table__pct {
		color: var(--color-text-muted);
	}

	.quota-table__bar {
		width: 40%;
		min-width: 6rem;
	}

	.quota-table__bar .ds-bar {
		margin-bottom: 0;
	}

	.quota-table__meta {
		text-align: right;
		white-space: nowrap;
	}

	.quota-table__unlimited {
		display: inline-block;
		padding: 2px var(--space-2);
		border-radius: var(--radius-full);
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
		font-size: var(--text-xs);
	}

	.kv {
		display: grid;
		grid-template-columns: auto 1fr;
		gap: var(--space-1) var(--space-4);
		margin: 0;
	}

	.kv dt {
		color: var(--color-text-muted);
	}

	.kv dd {
		margin: 0;
	}

	.badge {
		display: inline-block;
		padding: 0.05rem 0.4rem;
		border-radius: var(--radius-sm);
		font-size: var(--text-xs, 0.7rem);
		font-weight: var(--weight-semibold, 600);
		line-height: 1.4;
		vertical-align: middle;
	}

	.badge--env {
		margin-left: var(--space-2);
		background: var(--color-warning-bg);
		color: var(--color-warning-text);
	}

	.badge--oidc {
		background: var(--color-info-bg);
		color: var(--color-info-text);
		text-transform: uppercase;
		display: inline-flex;
		align-items: center;
		gap: 0.25rem;
	}

	.badge--local {
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
		text-transform: uppercase;
	}

	/*
	 * OPAQUE adoption chips — sit next to the auth-provider badge in
	 * the admin user table. Green = user has actually logged in via
	 * OPAQUE at least once (`opaque_migrated`); softer info shade =
	 * envelope on file but the OPAQUE handshake hasn't landed yet
	 * (`opaque_registered && !opaque_migrated`). The two-shade pattern
	 * matches the way `.badge--active` vs `.badge--inactive` split
	 * "success" from "neutral".
	 */
	/*
	 * `password` chip — neutral tone since the presence of a password
	 * is neither notably positive nor risky on its own; the OPAQUE
	 * chip next to it (if present) carries the "hardened" signal.
	 */
	.badge--password {
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
		text-transform: uppercase;
		display: inline-flex;
		align-items: center;
		gap: 0.25rem;
	}

	/*
	 * `mail` chip — user has no password / no OPAQUE / no SSO, so
	 * their only login path is a magic-link to their mailbox.
	 * Warning tone because for an internal account it's usually an
	 * in-flight-invitation state the operator wants to notice; for
	 * externals (grant-only) it's the by-design default. Named
	 * `mail` (not `passwordless`) so the label describes the actual
	 * channel the operator has to care about.
	 */
	.badge--mail {
		background: var(--color-warning-bg, var(--color-bg-muted));
		color: var(--color-warning-text, var(--color-text-muted));
		text-transform: uppercase;
		display: inline-flex;
		align-items: center;
		gap: 0.25rem;
	}

	.badge--opaque {
		background: var(--color-success-bg);
		color: var(--color-success-text);
		text-transform: uppercase;
		display: inline-flex;
		align-items: center;
		gap: 0.25rem;
	}

	.badge--opaque-registered {
		background: var(--color-info-bg);
		color: var(--color-info-text);
		text-transform: uppercase;
		display: inline-flex;
		align-items: center;
		gap: 0.25rem;
	}

	.badge--active {
		background: var(--color-success-bg);
		color: var(--color-success-text);
	}

	.badge--inactive {
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
	}

	.badge--admin {
		background: var(--color-info-bg);
		color: var(--color-info-text);
		text-transform: uppercase;
		display: inline-flex;
		align-items: center;
		gap: 0.25rem;
	}

	.badge--user {
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
		text-transform: uppercase;
	}

	.badge--self {
		margin-left: var(--space-1);
		background: var(--color-warning-bg);
		color: var(--color-warning-text);
		text-transform: uppercase;
	}

	/* Presence dot in the sessions-table Status column — filled green
	   when the row is `is_online` (a request landed in the last 5 min),
	   outlined grey when the row is active-but-idle. Only rendered on
	   active rows: a revoked-but-recently-seen row must never flash
	   green post-revocation (see the markup guard `{#if s.is_active}`).
	   The dot sits BEFORE the `active` badge with a small gap, so the
	   Status cell reads left-to-right as `● active` when online and
	   `○ active` when idle.

	   Tokens: `--color-success-alt` / `--color-success-border` for the
	   filled fill is the same green used by `.badge--active`, keeping
	   the presence signal visually consistent with the lifecycle one
	   without stealing the badge's own colour treatment. Grey border
	   for the idle state uses the neutral `--color-border` token so
	   both themes (light + dark, driven by `light-dark(...)`) get a
	   readable contrast. Fixed 8px / 8px sizing — the dot is a signal,
	   not a click target, so relative units would over-scale on
	   larger UI densities. */
	.presence-dot {
		display: inline-block;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		margin-right: var(--space-1);
		vertical-align: middle;
		/* No border on the filled state, so both variants render at
		   the same 8×8 footprint (the outlined variant's 1px border
		   is inset via box-sizing: border-box below). */
		box-sizing: border-box;
	}

	.presence-dot--online {
		background: var(--color-success-alt);
	}

	.presence-dot--idle {
		background: transparent;
		border: 1px solid var(--color-border);
	}

	/* External / grant-only account marker. Sibling of `.badge--user`
	   in the same cell so the two stack horizontally; the accent
	   colour reuses `--color-warning-*` because "external" is the
	   same "attention needed" family as "you". */
	.badge--external {
		background: var(--color-warning-bg);
		color: var(--color-warning-text);
		text-transform: uppercase;
	}

	/* Wrapper for the (role + optional external) badge pair in the
	   users table. Row-flex + nowrap keeps the two on a single line
	   when the column narrows; the sibling badges themselves also
	   set `white-space: nowrap` so the ".EXTERNAL" label never
	   splits mid-word either. */
	.role-badges {
		display: flex;
		/* Stack role above the (rare) external tag so the two signals
		 * are unambiguous — one reads "admin\nexternal" instead of a
		 * horizontal row that could be misread as "admin external"
		 * (as-if externals-can-be-admin, which they can't). Narrow
		 * items align left so the badges keep a consistent left edge
		 * with the surrounding text. */
		flex-flow: column nowrap;
		align-items: flex-start;
		gap: var(--space-1, 0.25rem);
	}

	.role-badges .badge {
		white-space: nowrap;
	}

	/* Authentication column. The OIDC provider label is free-form
	   and can widen the column arbitrarily on hosts that use long
	   identifiers ("google-workspace-prod"). Cap the column width
	   and ellipsis the label so the row layout stays balanced; the
	   full provider is still reachable via the badge tooltip. */
	.auth-cell {
		max-width: 12ch;
	}

	.auth-cell .badge__label {
		display: inline-block;
		max-width: 8ch;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		vertical-align: bottom;
	}

	/* Enabled/disabled feature flag indicator on the dashboard cards. */
	.ds-flag {
		font-size: 1.125rem;
		font-weight: var(--weight-bold);
		color: var(--color-text-muted);
	}

	.ds-flag--on {
		color: var(--color-success-text);
	}

	.warn-card {
		display: flex;
		align-items: center;
		gap: var(--space-3);
	}

	.warn-card--warn {
		border-color: var(--color-warning-text);
		color: var(--color-warning-text);
	}

	.warn-card--danger {
		border-color: var(--color-error-text);
		color: var(--color-error-text);
	}

	/* Per-user storage-usage progress bar in the users table. */
	.quota-cell {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		min-width: 9rem;
	}

	.quota-bar {
		height: 6px;
		background: var(--color-bg-muted);
		border-radius: var(--radius-full);
		overflow: hidden;
	}

	.quota-fill {
		height: 100%;
		background: var(--color-success-text);
	}

	.quota-fill--warn {
		background: var(--color-warning-text);
	}

	.quota-fill--danger {
		background: var(--color-error-text);
	}

	.quota-input {
		display: flex;
		gap: var(--space-2);
	}

	.quota-input input {
		flex: 1;
	}

	/* Icon-only row actions with hover tooltips (title attr). */
	.icon-btn {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 2rem;
		height: 2rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-surface);
		color: var(--color-text);
		cursor: pointer;
	}

	.icon-btn:disabled {
		opacity: 0.45;
		cursor: not-allowed;
	}

	.icon-btn--danger {
		color: var(--color-error-text);
	}

	.icon-btn--success {
		color: var(--color-success-text);
	}

	.secret-hint {
		display: inline-flex;
		align-items: center;
		gap: 0.25rem;
		margin-top: var(--space-1);
		font-size: var(--text-sm);
		color: var(--color-success-text);
	}

	.registration-warning {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		margin-top: var(--space-3);
	}

	.alert--warn {
		padding: var(--space-2) var(--space-3);
		border-radius: var(--radius-md);
		background: var(--color-warning-bg);
		color: var(--color-warning-text);
	}

	/* Discovery / verify result panels. */
	.discovery-result {
		margin-top: var(--space-2);
		padding: var(--space-3);
		border-radius: var(--radius-md);
		border: 1px solid var(--color-border);
	}

	.discovery-result strong {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
	}

	.discovery-result--ok {
		border-color: var(--color-success-text);
		color: var(--color-success-text);
	}

	.discovery-result--fail {
		border-color: var(--color-error-text);
		color: var(--color-error-text);
	}

	.discovery-result .kv {
		margin-top: var(--space-2);
		color: var(--color-text);
	}

	.callback-row {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		flex-wrap: wrap;
	}

	.gen-key-fp {
		margin-top: var(--space-2);
		font-size: var(--text-sm);
	}

	.gen-key-fp code {
		font-family: var(--font-mono);
		color: var(--color-text-heading);
	}

	.maint-row {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		flex-wrap: wrap;
		margin-top: var(--space-3);
	}

	.maint-result {
		font-variant-numeric: tabular-nums;
	}

	.install-bar {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-3);
		padding: var(--space-3);
		border: 1px dashed var(--color-border);
		border-radius: var(--radius-md);
		margin-bottom: var(--space-3);
	}

	.install-bar .muted {
		display: block;
		font-size: var(--text-sm);
	}

	.install-bar .btn.disabled {
		opacity: 0.6;
		pointer-events: none;
	}

	.events {
		display: inline-block;
		min-width: 1.4rem;
		text-align: center;
		padding: 0 0.35rem;
		border-radius: var(--radius-pill, 999px);
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
	}

	.plugin-meta {
		margin-bottom: var(--space-4);
	}

	.event-tag {
		display: inline-block;
		margin: 0 0.15rem 0.15rem 0;
		padding: 0.05rem 0.35rem;
		border-radius: var(--radius-sm);
		background: var(--color-bg-muted);
	}

	.retention-form {
		border-top: 1px solid var(--color-border);
		padding-top: var(--space-3);
		margin-bottom: var(--space-3);
	}

	.retention-form h3 {
		margin: 0 0 var(--space-2);
		font-size: 1rem;
	}

	.mig-failed {
		margin-top: var(--space-2);
	}

	.mig-failed__list {
		max-height: 12rem;
		overflow: auto;
		padding: var(--space-2);
		background: var(--color-bg-muted);
		border-radius: var(--radius-sm);
		font-size: var(--text-xs, 0.75rem);
		white-space: pre-wrap;
		word-break: break-all;
	}

	.cutover-hint {
		margin-top: var(--space-3);
		padding: var(--space-3);
		border: 1px solid var(--color-warning-border, var(--color-border));
		border-radius: var(--radius-md);
		background: var(--color-warning-bg, var(--color-bg-muted));
	}

	.cutover-hint h3 {
		margin: 0 0 var(--space-2) 0;
		font-size: var(--text-base, 1rem);
	}

	.cutover-hint--readonly {
		border-color: var(--color-danger-border, var(--color-border));
		background: var(--color-danger-bg, var(--color-bg-muted));
	}

	/* On the readonly banner the danger-tinted background swallows
	   `.muted` (which is a light grey). Use the strong text color
	   instead so the body message stays legible in both themes.
	   `color-danger-text` if the design system publishes one, else
	   fall back to the regular text color which still meets WCAG
	   contrast against the muted-red/pink bg tokens. */
	.cutover-hint__readonly-body {
		margin: 0;
		color: var(--color-danger-text, var(--color-text));
	}

	.storage-content-stats {
		margin-bottom: var(--space-4);
	}

	.storage-content-stats h2 {
		margin: 0 0 var(--space-1);
	}

	.storage-content-stats__hint {
		margin: 0 0 var(--space-3);
		font-size: var(--text-sm);
	}

	.storage-content-stats__grid {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(9rem, 1fr));
		gap: var(--space-3);
		margin: 0;
	}

	.storage-content-stats__grid > div {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}

	.storage-content-stats__grid dt {
		font-size: var(--text-xs);
		color: var(--color-text-muted);
		text-transform: uppercase;
		letter-spacing: 0.03em;
	}

	.storage-content-stats__grid dd {
		margin: 0;
		font-size: var(--text-md);
		font-weight: var(--weight-semibold);
		color: var(--color-text-heading);
		font-variant-numeric: tabular-nums;
	}

	.entries-list {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
		margin-bottom: var(--space-3);
	}

	.entry-card {
		padding: var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg);
	}

	.entry-card--active {
		border-color: var(--color-accent, var(--color-border));
		box-shadow: 0 0 0 1px var(--color-accent, transparent) inset;
	}

	.entry-card__header {
		display: flex;
		justify-content: space-between;
		align-items: flex-start;
		gap: var(--space-3);
		flex-wrap: wrap;
		margin-bottom: var(--space-3);
	}

	.entry-card__title {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		flex-wrap: wrap;
	}

	.entry-card__name {
		font-size: var(--text-base, 1rem);
		font-weight: 600;
	}

	.entry-card__badge {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		padding: 0 var(--space-2);
		border-radius: var(--radius-sm);
		font-size: var(--text-xs, 0.75rem);
		color: var(--color-text-muted);
		background: var(--color-bg-muted);
	}

	.entry-card__badge--active {
		color: var(--color-success-text, var(--color-text));
		background: var(--color-success-bg, var(--color-bg-muted));
	}

	.entry-card__badge--encrypted {
		color: var(--color-text-muted);
	}

	.entry-card__actions {
		display: flex;
		gap: var(--space-2);
		flex-wrap: wrap;
		align-items: center;
		justify-content: flex-end;
	}

	/* Each button occupies a fixed min-width so the same slot on
	   the next card lines up vertically. `max-content` on the label
	   prevents the button from stretching taller than needed.
	   Placeholders keep their layout box but the pixels are gone
	   and the button is unclickable. */
	.entry-card__action-btn {
		min-width: 10rem;
		justify-content: center;
	}

	.entry-card__action-btn--placeholder {
		visibility: hidden;
		pointer-events: none;
	}

	.entry-card__grid {
		display: grid;
		grid-template-columns: max-content 1fr;
		gap: var(--space-1) var(--space-3);
		margin: 0;
		font-size: var(--text-sm, 0.875rem);
	}

	.entry-card__grid dt {
		color: var(--color-text-muted);
	}

	.entry-card__grid dd {
		margin: 0;
	}

	.entry-card__mono {
		font-family: var(--font-mono, monospace);
		font-size: var(--text-xs, 0.75rem);
		word-break: break-all;
	}

	.entry-card__test-result {
		margin-top: var(--space-3);
		padding-top: var(--space-2);
		border-top: 1px solid var(--color-border);
	}

	/* K3.7 pair-chain — one row per configured pair. Head is bolded
	   and gets an "← head" badge so the write pair pops out. Aligns
	   the fingerprint column so admins can eyeball-diff between
	   entries. */
	.entry-card__pairs {
		margin-top: var(--space-3);
		padding-top: var(--space-2);
		border-top: 1px solid var(--color-border);
	}

	.entry-card__pairs-title {
		font-size: var(--text-xs, 0.75rem);
		font-weight: var(--weight-semibold, 600);
		text-transform: uppercase;
		letter-spacing: 0.5px;
		color: var(--color-text-muted);
		margin: 0 0 var(--space-2);
	}

	.entry-card__pair-chain {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
	}

	.entry-card__pair {
		display: grid;
		grid-template-columns: 3rem 6.5rem 1fr auto;
		align-items: center;
		gap: var(--space-2);
		font-size: var(--text-sm);
	}

	.entry-card__pair-idx {
		color: var(--color-text-muted);
		font-variant-numeric: tabular-nums;
	}

	.entry-card__pair-cipher {
		color: var(--color-text);
	}

	.entry-card__pair-fp {
		font-family: var(--font-mono, monospace);
		font-size: var(--text-xs);
		color: var(--color-text-muted);
		word-break: keep-all;
	}

	.entry-card__pair--head .entry-card__pair-cipher,
	.entry-card__pair--head .entry-card__pair-fp {
		color: var(--color-text);
		font-weight: var(--weight-semibold, 600);
	}

	.entry-card__pair-head-badge {
		font-size: var(--text-xs);
		padding: 0 var(--space-1);
		background: var(--color-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-sm);
		color: var(--color-accent);
	}

	.entry-card__pairs-help {
		margin: var(--space-2) 0 0;
		font-size: var(--text-xs);
	}

	.mig-status {
		margin: var(--space-3) 0;
	}

	.mig-status button {
		margin-left: var(--space-2);
	}

	.smtp-test {
		display: flex;
		gap: var(--space-2);
	}

	.smtp-test input {
		flex: 1;
		padding: var(--space-2) var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
	}

	.status--ok {
		color: var(--color-success-text);
	}

	.admin {
		max-width: 64rem;
		margin: 0 auto;
		padding: 1.5rem 1rem;
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.bar {
		display: flex;
		justify-content: flex-end;
		align-items: center;
		gap: var(--space-3, 0.75rem);
		flex-wrap: wrap;
	}

	/* Sessions-panel toolbar items — filter input + include-revoked
	   checkbox pushed to the left, refresh button anchored right. */
	.bar__filter {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2, 0.5rem);
		margin-right: auto;
		font-size: 0.875rem;
	}

	.bar__filter input {
		padding: 0.375rem 0.5rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md, 4px);
		background: var(--color-bg-input, var(--color-bg));
		color: var(--color-text);
		font-family: var(--font-mono, monospace);
		font-size: 0.8125rem;
		min-width: 20ch;
	}

	.bar__toggle {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2, 0.5rem);
		font-size: 0.875rem;
		cursor: pointer;
	}

	/* Admin table user cell — UserVignette (avatar + name + email)
	   with the "you" badge parked to its right when this row belongs
	   to the caller. Flex + gap keeps them shoulder-to-shoulder
	   without collapsing on narrow columns. Shared across Users +
	   Sessions tabs; both benefit from the same avatar/name/email
	   presentation. */
	.user-vignette-cell {
		display: flex;
		align-items: center;
		gap: var(--space-2, 0.5rem);
		flex-wrap: wrap;
	}

	/* DPoP-bound cell: lock icon + short jkt prefix, kept tight so
	   the column doesn't inflate on wide viewports. */
	.bound-cell {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1, 0.25rem);
		font-size: 0.8125rem;
	}

	.table {
		width: 100%;
		border-collapse: collapse;
	}

	.table th,
	.table td {
		text-align: left;
		padding: 0.5rem 0.625rem;
		border-bottom: 1px solid var(--color-border);
		font-size: 0.875rem;
	}

	/* Row hover highlight — covers Users / Sessions / Drives (every
	   admin table renders through `.table`). Header rows and empty-
	   state rows are excluded via `tbody` scoping. `transition` keeps
	   the tint from feeling twitchy on fast pointer movement. */
	.table tbody tr {
		transition: background-color 120ms ease;
	}

	.table tbody tr:hover {
		background-color: var(--color-bg-hover);
	}

	.user-cell {
		display: flex;
		flex-direction: column;
	}

	.muted {
		color: var(--color-text-muted);
		font-size: 0.8125rem;
	}

	.actions {
		display: flex;
		flex-wrap: wrap;
		gap: 0.5rem;
	}

	.pager {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 1rem;
	}

	.form {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}

	.form label {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		font-size: 0.875rem;
	}

	.form input,
	.form select {
		padding: 0.5rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
	}

	.btn {
		padding: 0.5rem 0.875rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-surface);
		color: var(--color-text);
		cursor: pointer;
	}

	.btn--primary {
		background: var(--color-primary);
		color: var(--color-text-light);
		border-color: transparent;
	}

	/* Destructive-action button. Kept red once enabled so the visual
	   weight of the action doesn't dilute when the typed-email gate
	   unlocks it — dimming to opacity only when disabled, not
	   swapping the palette. Used by the delete-user modal's Delete
	   button. */
	.btn--danger {
		background: var(--color-error-text);
		color: var(--color-text-light);
		border-color: transparent;
	}

	.btn--danger:hover:not(:disabled) {
		filter: brightness(0.92);
	}

	.btn--danger:disabled {
		opacity: 0.55;
		cursor: not-allowed;
	}

	.status {
		color: var(--color-text-muted);
		padding: 2rem 0;
		text-align: center;
	}

	.status--error {
		color: var(--color-error-text);
	}

	.link-btn {
		background: none;
		border: none;
		color: var(--color-primary);
		cursor: pointer;
		font-size: 0.8125rem;
	}

	.link-btn--danger {
		color: var(--color-error-text);
	}

	/* Drive-owner autocomplete dropdown inside the create-drive modal. */
	.drive-owner {
		position: relative;
	}

	.owner-suggest {
		list-style: none;
		margin: 0;
		padding: 0;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-surface);
		max-height: 14rem;
		overflow-y: auto;
	}

	.owner-suggest__row {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		width: 100%;
		padding: var(--space-2) var(--space-3);
		border: none;
		background: none;
		text-align: left;
		font: inherit;
		color: var(--color-text);
		cursor: pointer;
	}

	.owner-suggest__row:hover {
		background: var(--color-bg-muted);
	}

	.owner-suggest__label {
		flex: 1;
	}

	.owner-pick {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
	}

	/* Manage-owners modal — current-owners list with a remove affordance. */
	.owners-list__title {
		margin: var(--space-3) 0 var(--space-2);
		font-size: 0.95rem;
	}

	.owners-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
	}

	.owners-list__row {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
	}

	.owners-list__id {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-size: 0.8125rem;
	}

	/* Inline group representation in the owners list — mirrors
	   UserVignette's avatar+text shape so the rows line up visually
	   even though the data sources differ. */
	.owners-list__group {
		display: flex;
		flex: 1;
		min-width: 0;
		align-items: center;
		gap: var(--space-2);
	}

	.owners-list__group-icon {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 32px;
		height: 32px;
		border-radius: 50%;
		background: var(--color-bg-muted);
		color: var(--color-text);
		flex-shrink: 0;
	}

	.owners-list__group-name {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	/* Policy list styles moved to `PolicyList.svelte`. The modal now
	   embeds `<PolicyList bind:values={managePoliciesDraft} … />` and the
	   read-only summary on `/config/drive/{uuid}` reuses the same
	   component. */

	/* Drives table action cell — same shape as `.actions` plus a fixed
	   3-column grid so the [users] [policies] [delete] icons line up
	   vertically across rows regardless of which actions a given drive
	   supports. Inapplicable actions render as invisible placeholders
	   (see `.icon-btn--placeholder`). */
	/* Users tab actions cell — five fixed slots so icons stay column-
	   aligned across rows even when a row skips some actions (external
	   users skip quota-edit + reset-password; internals never get the
	   promote button). Prevents the last icon from wrapping to a new
	   line when a placeholder + all five buttons would together push
	   past the cell width. */
	.actions--user {
		display: grid;
		grid-template-columns: repeat(5, auto);
		justify-content: end;
		align-items: center;
		gap: var(--space-1, 0.25rem);
	}

	.actions--drive {
		display: grid;
		/* Four action slots per row: manage-owners, policies, edit-
		   quota, delete. Fixed columns keep icons aligned across
		   rows even when a row renders placeholders for
		   inapplicable actions (e.g. personal drives have no
		   owners roster). */
		grid-template-columns: repeat(4, auto);
		justify-content: end;
		align-items: center;
	}

	.drive-kind-cell {
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: 0.15rem;
	}

	.drive-kind-cell__suffix {
		font-size: 0.85em;
	}

	.icon-btn--placeholder {
		/* Reserves the column width without rendering anything
		   interactive. `visibility: hidden` keeps layout intact;
		   pointer-events:none stops accidental focus from keyboard
		   nav. */
		visibility: hidden;
		pointer-events: none;
	}
</style>
