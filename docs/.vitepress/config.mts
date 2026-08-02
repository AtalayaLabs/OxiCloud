import { defineConfig } from "vitepress";
import type MarkdownIt from "markdown-it";

// When a doc page links to a source-tree file with a relative path
// escaping the docs directory (e.g. `[build.rs](../build.rs)` or
// `[handler](../src/…/file_handler.rs)`), VitePress rightly flags it
// as a dead link — those files aren't part of the built site. On the
// deployed site the click would 404. Locally in an editor / on
// GitHub, though, those relative paths ARE useful — they let a
// reader jump to the actual source.
//
// This plugin bridges the two: at build time, links whose href
// starts with `../` get rewritten to their equivalent GitHub blob
// URL. Source stays terse and useful in-editor; deployed site links
// resolve on GitHub instead of 404ing.
//
// Same repo the `editLink` already points at + the main branch —
// keep in sync if the canonical repo ever moves.
const GITHUB_REPO = "DioCrafts/OxiCloud";
const GITHUB_BRANCH = "main";

function rewriteSourceTreeLinks(md: MarkdownIt): void {
  const defaultRender =
    md.renderer.rules.link_open ??
    ((tokens, idx, options, _env, self) =>
      self.renderToken(tokens, idx, options));
  md.renderer.rules.link_open = (tokens, idx, options, env, self) => {
    const token = tokens[idx];
    const hrefIdx = token.attrIndex("href");
    if (hrefIdx >= 0) {
      const href = token.attrs![hrefIdx][1];
      // Match paths that escape the docs directory. Only `../` prefix
      // is targeted — leaves in-docs relative links alone so real
      // dead links still get caught.
      if (href.startsWith("../")) {
        // Strip the leading `../` — everything after is the repo-root
        // relative path. `#L123` line anchors on GitHub are preserved
        // as-is because the URL fragment isn't touched.
        const path = href.slice(3);
        token.attrs![hrefIdx][1] =
          `https://github.com/${GITHUB_REPO}/blob/${GITHUB_BRANCH}/${path}`;
        // Open in a new tab since it now leaves the doc site.
        token.attrSet("target", "_blank");
        token.attrSet("rel", "noopener noreferrer");
      }
    }
    return defaultRender(tokens, idx, options, env, self);
  };
}

export default defineConfig({
  title: "OxiCloud",
  description: "Self-hosted cloud storage, calendar & contacts — blazingly fast",

  base: "/OxiCloud/",

  sitemap: {
    hostname: "https://diocrafts.github.io/OxiCloud",
    lastmodDateOnly: false,
  },

  // Internal planning notes (docs/plan/*) and AI hand-off prompts
  // aren't user-facing docs and contain raw Rust/pseudocode whose
  // generics (Option<String>, Vec<SubjectGroup>, …) trip Vue's
  // template parser ("Element is missing end tag" because <String>,
  // <SubjectGroup>, etc. look like unclosed HTML tags). Keep them
  // in-tree for engineering reference but skip them at site-build time.
  srcExclude: [
    "plan/**",
    "**/*.prompt",
  ],

  markdown: {
    image: {
      lazyLoading: true,
    },
    // Rewrite `../src/…`, `../build.rs`, etc. → GitHub blob URLs at
    // build time. See the `rewriteSourceTreeLinks` docstring above.
    config: (md) => rewriteSourceTreeLinks(md),
  },

  lastUpdated: true,

  ignoreDeadLinks: [
    /^https?:\/\/localhost/,
  ],

  locales: {
    root: {
      label: "English",
      lang: "en",
    },
  },

  head: [
    ["link", { rel: "icon", href: "/OxiCloud/logo.svg" }],
  ],

  themeConfig: {
    logo: "/logo.svg",

    search: {
      provider: "local",
    },

    editLink: {
      pattern: "https://github.com/DioCrafts/OxiCloud/tree/main/docs/:path",
      text: "Edit this page on GitHub",
    },

    nav: [
      { text: "Home", link: "/" },
      { text: "Guide", link: "/guide/" },
      { text: "Configuration", link: "/config/" },
      { text: "FAQ", link: "/faq" },
    ],

    sidebar: {
      "/": [
        {
          text: "Introduction",
          items: [
            { text: "What is OxiCloud?", link: "/guide/" },
            { text: "Quick Start", link: "/guide/installation" },
          ],
        },
        {
          text: "Configuration",
          items: [
            { text: "Deployment & Docker", link: "/config/deployment" },
            { text: "Environment Variables", link: "/config/env" },
            { text: "Storage Fine Tuning", link: "/config/storage-fine-tuning" },
            { text: "Authentication", link: "/config/authentication" },
            { text: "OIDC / SSO", link: "/config/oidc" },
            { text: "OIDC Config Examples", link: "/config/oidc-config-examples" },
            { text: "Admin Settings", link: "/config/admin-settings" },
            { text: "WOPI (Office Editing)", link: "/config/wopi" },
          ],
        },
        {
          text: "Features",
          items: [
            { text: "Drives", link: "/guide/drives" },
            { text: "Sharing", link: "/guide/sharing" },
            { text: "WebDAV", link: "/guide/webdav" },
            { text: "CalDAV & CardDAV", link: "/guide/caldav-carddav" },
            { text: "DAV Client Setup", link: "/guide/dav-client-setup" },
            { text: "Chunked Uploads", link: "/guide/chunked-uploads" },
            { text: "Batch Operations", link: "/guide/batch-operations" },
            { text: "Deduplication", link: "/guide/deduplication" },
            { text: "Favorites & Recent", link: "/guide/favorites-and-recent" },
            { text: "Search", link: "/guide/search" },
            { text: "Thumbnails & Transcoding", link: "/guide/thumbnails-and-transcoding" },
            { text: "Trash & Recycle Bin", link: "/guide/trash" },
            { text: "ZIP & Compression", link: "/guide/zip-and-compression" },
            { text: "Internationalization", link: "/guide/i18n" },
            { text: "Backend Storage", link: "/guide/backend-storage" },
          ],
        },
        {
          text: "Architecture",
          items: [
            { text: "Internal Architecture", link: "/architecture/" },
            { text: "Caching", link: "/architecture/caching" },
            { text: "Resource Listing API", link: "/architecture/resource-listing" },
            { text: "Storage Safety", link: "/architecture/file-system-safety" },
            { text: "Database Transactions", link: "/architecture/database-transactions" },
            { text: "ReBAC Authorization", link: "/architecture/rebac-authorization" },
            { text: "Share Integration", link: "/architecture/share-integration" },
            { text: "Storage Quotas", link: "/architecture/storage-quotas" },
            { text: "Backend Storage", link: "/architecture/backend-storage" },
            { text: "File and Blob lifecycle", link: "/architecture/file-and-blob-lifecycle" },
            { text: "ReBAC & Authorization", link: "/architecture/rebac-authorization" },
            { text: "User lifecycle", link: "/architecture/user-lifecycle" },
            { text: "Authentication model", link: "/architecture/auth-model" },
            { text: "Magic-link auth", link: "/architecture/magic-link-auth" },
            { text: "Background jobs", link: "/architecture/jobs" },
          ],
        },
        { text: "FAQ", link: "/faq" },
      ],
    },

    socialLinks: [
      { icon: "github", link: "https://github.com/DioCrafts/OxiCloud" },
    ],

    footer: {
      message: "Released under the MIT License.",
      copyright: "Copyright © 2025-present DioCrafts",
    },
  },
});
