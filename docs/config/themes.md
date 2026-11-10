# Global appearance

The system operator installs trusted CSS files on the server. Application
administrators choose a theme or refresh the catalogue. There is no CSS editor,
upload endpoint or file mutation in the application.

## Shipped themes and operator themes

`default` and `modern-pro` are embedded in the server, including Docker images
and standalone binaries. They work without the source tree and independently of
the current directory. The frontend also includes the default tokens as a local
fallback if the active stylesheet cannot be downloaded.

Install another theme as `<themes_path>/<id>/theme.css`:

```css
:root {
    --color-accent: light-dark(#123456, #abcdef);
}

/* Component overrides are allowed in this same file. */
.page-title {
    letter-spacing: 0.02em;
}
```

The default operator directory is `./resources/themes/installed`, relative to the
server working directory. Set `OXICLOUD_THEMES_PATH` for another directory, or
`[server] themes_path` in TOML. An explicitly configured root must exist and be
readable; an absent implicit root simply leaves the shipped themes available.

An operator file named `modern-pro/theme.css` overrides the shipped Modern Pro.
The identifier `default` is reserved and cannot be overridden. In a source
checkout the included Modern Pro file also serves as the editable override.
Removing an override restores the shipped version; removing a custom selected
folder falls back to `default` with a visible warning, preserving the saved ID.

For Docker, mount only the operator directory read-only:

```yaml
services:
  oxicloud:
    environment:
      OXICLOUD_THEMES_PATH: /app/themes
    volumes:
      - /srv/oxicloud/themes:/app/themes:ro
```

A read-only mount prevents the application from changing files; it does not
prevent the operator or another privileged host process from changing them.

## Selection, refresh and recovery

Use **Admin → Appearance** to select a theme. A saved selection takes priority
over `OXICLOUD_DEFAULT_THEME` (`[server] default_theme`), which otherwise defaults
to `default`. Environment takes priority over TOML. Selection is stored in
`auth.admin_settings` under `appearance.active_theme`, category `appearance`,
nonsecret, with the administrator's `updated_by`.

**Refresh** rereads operator files and atomically publishes the catalogue and
active CSS. Directory failures or an unreadable active theme preserve the
previous snapshot. Inactive unreadable entries are excluded with diagnostics.
Selection and refresh share one mutation lock; CSS requests never wait for disk
or SQL. Files are never read or compressed per HTTP request. Existing pages
apply changed CSS on their next full reload, not on SPA navigation.

The application does not validate CSS syntax, restrict selectors, or infer token
types. CSS is served unchanged as UTF-8, and the browser interprets it. A theme
can hide controls, break contrast, or make the administration page unusable.

To recover, set the following on the server and restart:

```dotenv
OXICLOUD_FORCE_DEFAULT_THEME=true
```

Or use `[server] force_default_theme = true` in TOML. This bypasses operator file
loading and forces the built-in default even when another theme is saved.
Selection is disabled and the saved choice is not changed. Repair the files,
clear the override and restart to restore the saved selection. Database failures
remain startup errors; this switch is an appearance recovery mechanism.

This version supports one application process per deployment. Processes sharing
a database do not synchronize their in-memory snapshots. Persistence happens
before publication. If a database acknowledgement is lost, a write may have
committed even though the API reports failure; the previous public snapshot is
retained. Retry the selection, refresh, or restart to reconcile confirmed state.
Dropping an HTTP connection does not cancel an in-flight mutation task.

## CSS and filesystem contract

- One `theme.css` per folder, UTF-8, at most 256 KiB; at most 64 root entries.
- IDs contain 1–64 ASCII lowercase letters, digits or hyphens and start with a
  letter or digit. Files must be regular files. Symlinks and special files are
  refused, including while traversing directory components.
- Reads are bounded using open descriptors. The server never writes, deletes,
  extracts or repairs theme files. Secure descriptor-relative loading targets
  Unix. Root paths cannot contain `..`; use an absolute path instead.
- Variables in `resources/themes/tokens.css` are a customization convention, not
  an allowlist. New components or variables need no backend registration. Prefer
  variable overrides; component selectors depend on markup and may need updates
  when upgrading OxiCloud.
- Operator CSS loads after the initial bundle styles. Normal CSS specificity
  still applies; Svelte component styles may require a more specific selector.
- CSS rules, functions, `@media`, `@import` and `!important` are passed through.
  Only install CSS you trust. The existing CSP stays unchanged; browser requests
  for styles, fonts and images remain subject to it. The server does not fetch
  URLs referenced by a theme.
- The theme directory is not a static file host. Additional local assets and
  stylesheets are not exposed. Relative CSS URLs resolve from `/themes/active.css`,
  not from the filesystem folder. Use existing public assets or CSP-permitted
  URLs. A separate `custom.css` is not loaded; put component rules in `theme.css`.

## Interfaces

`GET`/`HEAD {base}/themes/active.css` are public, with CSS Content-Type, `nosniff`,
a weak content ETag, `Vary: Accept-Encoding`, and `Cache-Control: no-cache`.
Brotli/gzip variants are prepared on catalogue load. Conditional requests return
304 without a body. Unknown `/themes` paths return 404, never the SPA shell.

`GET`/`PUT {base}/api/admin/settings/appearance` and
`POST {base}/api/admin/settings/appearance/reload` require a live active internal
administrator or owner and retain the existing session/CSRF/DPoP protections.
PUT accepts `{ "theme_id": "modern-pro" }`. Responses contain catalogue IDs,
selection and diagnostics, never CSS or absolute server paths.

Warnings: `selected_theme_unavailable` means a saved ID is missing;
`forced_default` means recovery mode is active. `invalid_theme` diagnostics mean
an entry could not be loaded (ID, filesystem, encoding or size), not that its CSS
was semantically validated. Detailed failures are logged on the server.
