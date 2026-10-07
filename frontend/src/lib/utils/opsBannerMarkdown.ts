/**
 * Strict-subset markdown renderer for operator-authored banners.
 *
 * One module, one purpose. Supports only what an ops banner should
 * ever need: inline emphasis + code + links + line breaks. Everything
 * else (headings, tables, images, HTML blocks, raw HTML) is escaped
 * through as literal text so an operator typing `<script>` sees
 * `<script>` on the banner, not a running script.
 *
 * Why not import marked/markdown-it + dompurify:
 * - The subset here is <80 lines of code to render and test.
 * - A sanitizer bug in a third-party lib is a surface we can't audit
 *   on our schedule. Our own tiny renderer has every escape rule
 *   explicit at the point of substitution.
 * - Banner markdown is admin-authored, so the XSS blast radius is
 *   small, but defence-in-depth matters — a compromised admin
 *   account should not escalate to full XSS across every session.
 *
 * ## Supported syntax
 *
 * - `**bold**` → `<strong>…</strong>`
 * - `*italic*` → `<em>…</em>` (also `_italic_` for Unicode-safe
 *   authoring where `*` is awkward)
 * - `` `code` `` → `<code>…</code>`
 * - `[label](url)` → `<a href="…" target="_blank" rel="noopener noreferrer">label</a>`
 *   — URL MUST start with `http://`, `https://`, or `mailto:`.
 *   Anything else (`javascript:`, `data:`, relative paths) renders
 *   as plain text `[label](url)`.
 * - Line break between non-empty lines → `<br>`.
 *
 * NOT supported (will render as literal text):
 * - Headings (`#`, `##`, …)
 * - Lists (`-`, `1.`)
 * - Tables, images, blockquotes, horizontal rules
 * - Raw HTML tags of any kind
 * - Nested emphasis (`***bold-italic***`) — renders as `*bold-italic*`
 *
 * The output is a string of safe HTML. Callers set `innerHTML`.
 */

/** Characters that would otherwise be interpretable as HTML. */
const HTML_ESCAPE: Record<string, string> = {
	'&': '&amp;',
	'<': '&lt;',
	'>': '&gt;',
	'"': '&quot;',
	"'": '&#39;'
};

function escapeHtml(s: string): string {
	return s.replace(/[&<>"']/g, (c) => HTML_ESCAPE[c] ?? c);
}

/** URL schemes allowed in `[label](url)`. */
const SAFE_URL_RE = /^(?:https?:\/\/|mailto:)/i;

/**
 * Render ops-banner markdown to safe HTML. Pure function, no DOM
 * access, idempotent.
 */
export function renderOpsBannerMarkdown(src: string): string {
	const lines = src.split(/\r?\n/);
	const rendered = lines.map(renderInline);
	// Trim trailing empty lines, then join with <br>. Collapse
	// consecutive empty lines down to one <br> — a banner is at most
	// a few sentences; double-breaks are visual noise.
	const trimmed: string[] = [];
	let lastEmpty = false;
	for (const line of rendered) {
		const isEmpty = line.trim() === '';
		if (isEmpty && lastEmpty) continue;
		trimmed.push(line);
		lastEmpty = isEmpty;
	}
	while (trimmed.length > 0 && trimmed[trimmed.length - 1]?.trim() === '') trimmed.pop();
	return trimmed.join('<br>');
}

// Placeholder tokens for the two-pass renderer. The sequence
// `~OXIPH{n}~` is unlikely to appear in operator-authored banner
// text, and the inline-code / link grammars don't produce it.
// If an operator ever posts a banner containing literal
// `~OXIPH0~`, that span will be swallowed — accept that in
// exchange for an obvious, lint-friendly delimiter choice.
const PH_PREFIX = '~OXIPH';
const PH_SUFFIX = '~';
const PH_RE = /~OXIPH(\d+)~/g;

/**
 * Render ONE line of banner markdown. Private — only called by
 * `renderOpsBannerMarkdown`. Order of operations is:
 *   1. Peel out `[label](url)` and `` `code` `` into protected
 *      placeholders — their content shouldn't be inline-escaped.
 *   2. HTML-escape the remaining text.
 *   3. Re-apply `**bold**` / `*italic*` on the escaped text.
 *   4. Substitute the placeholders with their final HTML.
 *
 * The placeholder trick keeps `**` INSIDE a code span from being
 * read as bold.
 */
function renderInline(line: string): string {
	const placeholders: string[] = [];
	const protect = (html: string): string => {
		const token = `${PH_PREFIX}${placeholders.length}${PH_SUFFIX}`;
		placeholders.push(html);
		return token;
	};

	// 1a. Code spans — greediest "longest run of non-backticks between
	//     single backticks". Captured FIRST so emphasis / links inside
	//     `` `like **this` `` stay literal.
	let out = line.replace(/`([^`]+)`/g, (_m, code) => protect(`<code>${escapeHtml(code)}</code>`));

	// 1b. Links. Label is escaped as text; URL is validated against
	//     the safe-scheme regex. Unsafe → unescaped original stays
	//     as literal text (visible to the operator so they can fix
	//     the URL and repost).
	out = out.replace(/\[([^\]]+)\]\(([^)]+)\)/g, (match, label: string, url: string) => {
		const trimmed = url.trim();
		if (!SAFE_URL_RE.test(trimmed)) return escapeHtml(match);
		const safeHref = escapeHtml(trimmed);
		const safeLabel = escapeHtml(label);
		return protect(
			`<a href="${safeHref}" target="_blank" rel="noopener noreferrer">${safeLabel}</a>`
		);
	});

	// 2. Escape whatever remains. Placeholder tokens are composed
	//    of printable chars `~` + `OXIPH` + digits + `~` and
	//    survive HTML-escape unchanged.
	out = escapeHtml(out);

	// 3. Emphasis — bold first (two chars) so `**text**` doesn't match
	//    italic's single-char rule. Non-greedy to prevent one bold
	//    span from swallowing the whole line.
	out = out.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
	out = out.replace(/\*([^*]+)\*/g, '<em>$1</em>');
	out = out.replace(/_([^_]+)_/g, '<em>$1</em>');

	// 4. Restore placeholders.
	return out.replace(PH_RE, (_m, idx: string) => placeholders[Number(idx)] ?? '');
}
