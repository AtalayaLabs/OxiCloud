/**
 * Shared "is this filename a text-shaped file?" test.
 *
 * The extension list is the sibling of `utils/display.ts`'s
 * `ICON_BY_EXT` — its `file-code`, `terminal` and `file-alt` groups are
 * the same "code / script / prose" families this regex accepts. Both
 * used to mirror a server-side classifier
 * (`display_helpers.rs::icon_class_with_ext`) by comment alone; that
 * classifier is gone and the icon table now lives next door, so
 * `display.test.ts` asserts that every extension here has a glyph
 * there. Two consumers today:
 *
 *   1. `FileViewer.svelte` decides whether to mount the collab editor
 *      (the alternative branches — video / octet-stream — mis-play
 *      code files).
 *   2. The "New file" action in `routes/files/[...path]/+page.svelte`
 *      warns before creating a file whose extension isn't on this list
 *      (`test.jpg` should almost certainly not go through the collab
 *      editor).
 *
 * Adding an extension: update the regex here AND `ICON_BY_EXT` in
 * `utils/display.ts`, or `display.test.ts` fails — which is how the
 * missing `.log` glyph was found when that assertion was first written.
 * (The follow-up in `project_mime_guess_overrides_pending` removes
 * this duplication entirely — the FE will fall back to a
 * `mime.startsWith('text/')` check once the server-side MIME table
 * covers the same set.)
 */
export const TEXTY_EXT_RE =
	/\.(md|markdown|rst|txt|log|js|jsx|mjs|cjs|ts|tsx|py|pyw|rs|go|java|kt|kts|scala|c|h|cpp|hpp|cc|cxx|cs|rb|php|swift|r|lua|pl|pm|html|htm|css|scss|sass|less|json|xml|yaml|yml|toml|ini|cfg|conf|sql|graphql|proto|vue|svelte|sh|bash|zsh|fish|ps1|bat|cmd)$/i;

/** True when `name`'s extension is in the text/code allow-list.
 *  Case-insensitive; matches on trailing extension only. Names with
 *  no extension return `false` — a `Makefile` / `Dockerfile` / bare
 *  `README` doesn't pass by name alone, though the server-side
 *  content-sniffing follow-up may relax that later. */
export function isTextyFilename(name: string): boolean {
	return TEXTY_EXT_RE.test(name);
}
