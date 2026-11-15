# Installing and managing themes

A theme is a folder containing one `index.css`. Its folder name is displayed
unchanged in **Admin → Appearance**. The selected theme applies to everyone;
light, dark and system mode remain individual preferences.

## Create a theme

Themes live in `./resources/themes`, relative to the server's working directory:

```text
resources/themes/
└── my-theme/
    └── index.css
```

Put all your CSS in `index.css`, for example:

```css
:root {
    --color-accent: light-dark(#365b96, #45618b);
}
```

Use `resources/themes/tokens.css` as a reference for variables. No build step is
needed: OxiCloud serves `index.css` unchanged and does not load sibling files.
Images and fonts must already be publicly available; theme folders are not asset hosts.
Relative URLs resolve against `/themes/active.css` (under the deployment's base
path), not against the theme folder. Do not put secrets in CSS: the active
stylesheet is public, including on sign-in pages. External images, fonts and
imports may contact third parties and remain subject to the site's Content
Security Policy; prefer assets hosted on your own instance or infrastructure.

Folder names use 1–64 lowercase ASCII letters, digits or hyphens, starting with a
letter or digit. `index.css` must be UTF-8 and at most 256 KiB. The catalogue accepts
up to 64 theme entries in addition to the built-in default. Invalid directories
and symlinks count towards this limit; regular files at the catalogue root are
ignored. Files must be readable by the service; symlinks are refused.
The catalogue path and its parent directories must also be free of symlinks and
`..` components.

Slate and Basalt are supplied in Docker images and release archives. The reserved `default`
theme is built into the server and always available as a fallback.

## Install, modify or remove

1. Copy your theme folder to the catalogue using SSH, SCP or SFTP.
2. Click **Admin → Appearance → Refresh**, select the theme and save.
3. Reload the page to apply it.

To modify a theme, edit its `index.css`, finish saving, then refresh the catalogue
and reload the page. To remove it, select `default`, move the folder outside the
catalogue, then refresh. Keep backups outside the catalogue.

CSS changes need no server restart. If the active file cannot be loaded during
refresh, the previous working version is retained; check server logs for details.

For a persistent external catalogue, set `OXICLOUD_THEMES_PATH` to an existing
directory and restart once. Keep customized folders across upgrades. See
[environment configuration](https://AtalayaLabs.github.io/OxiCloud/config/env.html#global-appearance) for theme settings. The built-in `default` theme is always the initial theme and fallback.

## Docker

Prepare a host directory, copy Slate into it if desired, and mount it read-only:

```yaml
services:
  oxicloud:
    environment:
      OXICLOUD_THEMES_PATH: /app/resources/themes
    volumes:
      - /srv/oxicloud/themes:/app/resources/themes:ro
```

The mount replaces the image's catalogue; an empty directory leaves only `default`.
Edit themes on the host, then refresh in administration. These files survive
container updates.

## Recovery

Only install trusted CSS. If a theme makes administration unusable, set
`OXICLOUD_FORCE_DEFAULT_THEME=true` and restart. Repair the file, clear this setting
and restart again. The saved selection is preserved.

## Developer maintenance

`resources/themes/tokens.css` is the single source for built-in visual defaults.
It is embedded in the Rust binary and included in the SPA and generated
`askama-common.css`; changing these defaults requires rebuilding both the
frontend and server. Installed `index.css` files instead require only a catalogue
refresh. Structural tokens and accessibility overrides remain in the frontend's
`base/variables.css` and `base/a11y.css`. Prefer overriding semantic tokens;
component selectors depend on application markup and may need updates between
releases. OxiCloud does not validate CSS syntax or guarantee contrast for custom
themes. Test light, dark, system and increased-contrast modes.

The loader bounds and reads files at startup or explicit refresh. `ThemeService`
authorizes administrators, serializes changes and persists the selection before
publishing an immutable snapshot. Public CSS requests read only that snapshot,
without SQL or filesystem access. Refresh failures retain the previous snapshot;
removing a selected theme falls back to `default` without erasing its saved ID.
Selection is local to each server process after publication: deployments with
multiple processes must refresh each process to observe a changed saved choice.

The public `GET`/`HEAD /themes/active.css` route uses a content-derived weak ETag
and `Cache-Control: no-cache`. The administrative API is
`GET`/`PUT /api/admin/settings/appearance` (PUT body: `{ "theme_id": "slate" }`)
and `POST /api/admin/settings/appearance/reload`. Responses report the available,
configured, saved and active IDs separately, plus recovery/fallback warnings and
catalogue diagnostics. Keep the OpenAPI spec and generated frontend types in sync
when changing these contracts.

Run `just front-design` for locale completeness and token/brand tooling,
`cargo test theme`, the frontend's `npm run test:unit`,
`bash tests/api/run.sh themes` and the Playwright `themes.spec.ts` and
`photos.spec.ts` suites. The Rust integration test also checks persistence across
service reconstruction. Browser layout tests intercept theme CSS to isolate
palettes; the selection test uses the real admin UI and public route. Build the
instrumented SPA before Playwright, and finish that build before running Rust
checks with `--all-features`, which embed the generated assets.
