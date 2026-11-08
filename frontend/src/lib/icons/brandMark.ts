/**
 * The OxiCloud cloud glyph, as path data.
 *
 * This exists because the same path is currently hand-inlined at eleven
 * places across the SPA and the Askama templates — `BrandMark.svelte`,
 * `AboutDialog.svelte`, the `login` / `nextcloud/login` / `upgrade` /
 * `device` routes, four `magic_link` pages and `nextcloud/drive_picker`.
 * Anything NEW that draws the mark imports it from here instead of adding
 * copy twelve; the existing eleven are a separate cleanup.
 *
 * Note this is the bare glyph, authored for `viewBox="95 67 320 320"` and
 * coloured by whatever draws it. It is NOT the same artwork as
 * `static/logo/logo-plain.svg`, which is the app-icon form: a 500×500 box
 * with a background circle and the fill baked in, used for favicons, the
 * web manifest and the no-JavaScript fallback.
 *
 * Two consumers cannot import it and keep their own literal, because they
 * render before or without any bundle: the boot splash in `src/app.html`
 * and the Askama templates. `AppSplash.test.ts` compares the shell's copy
 * against this constant and fails if they drift.
 *
 * The path is also authored counter-clockwise (bottom-right → right → top
 * → left → bottom), which matters when animating along it: a dash needs a
 * POSITIVE `stroke-dashoffset` target to travel clockwise.
 */
export const BRAND_MARK_PATH =
	'M345 310c32 0 58-26 58-58s-26-58-58-58c-6.2 0-12 0.9-17.5 2.7C318 166 289 143 255 143c-34.3 0-63.1 22.6-73 53.7C176.9 195.7 171 195 165 195c-32 0-58 26-58 58s26 58 58 58h180z';

/** The viewBox the glyph is authored for. */
export const BRAND_MARK_VIEWBOX = '95 67 320 320';
