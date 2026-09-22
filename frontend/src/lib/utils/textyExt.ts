/**
 * Shared "is this filename a text-shaped file?" test.
 *
 * The extension list mirrors the server-side
 * `display_helpers.rs::icon_special_class_with_ext` "code / script /
 * prose" families — anything the backend classifies as text-shaped for
 * its icon picker qualifies here too. Two consumers today:
 *
 *   1. `FileViewer.svelte` decides whether to mount the collab editor
 *      (the alternative branches — video / octet-stream — mis-play
 *      code files).
 *   2. The "New file" action in `routes/files/[...path]/+page.svelte`
 *      warns before creating a file whose extension isn't on this list
 *      (`test.jpg` should almost certainly not go through the collab
 *      editor).
 *
 * Adding an extension: update the regex here AND
 * `display_helpers.rs::icon_special_class_with_ext` in the same
 * change so the FE gate and the backend classifier stay aligned.
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
