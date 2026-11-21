/** Build Chromium's `DownloadURL` drag payload. Colons delimit its three fields. */
export function downloadUrlPayload(name: string, absoluteUrl: string): string {
	const safeName = name.replaceAll(':', '_');
	return `application/octet-stream:${safeName}:${absoluteUrl}`;
}
