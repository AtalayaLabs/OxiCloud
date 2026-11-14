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

Folder names use 1–64 lowercase ASCII letters, digits or hyphens, starting with a
letter or digit. `index.css` must be UTF-8 and at most 256 KiB. Up to 64 themes are
supported. Files must be readable by the service; symlinks are refused.

Slate is supplied in Docker images and release archives. The reserved `default`
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
[environment configuration](env.md#global-appearance) for theme settings, including
`OXICLOUD_DEFAULT_THEME`; update that setting before removing its configured theme.

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
