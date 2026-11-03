/**
 * Format a byte count as a human-readable size string.
 * Seed utility to validate the unit-test harness; the full formatter set is
 * ported from static/js/core/formatters.js in Phase 1.
 */
export function formatBytes(bytes: number, decimals = 1): string {
	if (!Number.isFinite(bytes) || bytes < 0) return '—';
	if (bytes === 0) return '0 B';
	const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
	const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
	const value = bytes / Math.pow(1024, i);
	return `${value.toFixed(i === 0 ? 0 : decimals)} ${units[i]}`;
}

/** Storage bar fill, clamped to 0–100. Unlimited quotas render as 0. */
export function storagePercent(used: number, quota: number): number {
	if (quota <= 0) return 0;
	return Math.min(100, (used / quota) * 100);
}
