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
to `default`. Environment takes priority over TOML.

**Refresh** rereads installed themes without restarting the server. Directory
failures or an unreadable active theme retain the previous catalogue; invalid
inactive entries are excluded with diagnostics. After selecting or refreshing,
reload the page to apply the CSS. SPA navigation alone does not apply changes.

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
a database do not synchronize their theme catalogues or active selections.
If saving fails, check the server logs and retry, refresh, or restart.

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
  stylesheets are not exposed. Relative CSS URLs resolve from `{base}/themes/active.css`
  (including any deployment prefix), not from the filesystem folder. Use existing
  public assets or CSP-permitted URLs. A separate `custom.css` is not loaded; put component rules in `theme.css`.

## Diagnostics

The appearance page reports an unavailable saved theme or active recovery mode.
Excluded entries mean their ID, filesystem access, encoding or size was invalid;
the server does not validate CSS syntax. Check server logs for details.
