---
aside: false
outline: false
layout: page
---

<script setup>
import { withBase } from "vitepress";

// Redoc is React-based, so we don't embed it as a live component
// inside VitePress (Vue). Instead the Redocly CLI pre-builds a
// self-contained HTML file at `docs/public/api/openapi/index.html`
// during `prepare-specs.mjs`, and we iframe it here. Same pattern
// the AsyncAPI page uses — one static-generator model, one embed
// mechanism, easy to reason about.
//
// `withBase` honours `base: "/OxiCloud/"` from config.mts so the
// iframe URL resolves both locally (`/api/openapi/index.html`) and
// on GitHub Pages (`/OxiCloud/api/openapi/index.html`).
const iframeSrc = withBase("/api/openapi/index.html");
</script>

<iframe
  :src="iframeSrc"
  title="REST API reference"
  style="width: 100%; height: calc(100vh - 4rem); border: 0; background: var(--vp-c-bg);"
  loading="lazy"
></iframe>

<noscript>

# REST API

The interactive reference requires JavaScript. The raw OpenAPI JSON
is at [`/api/openapi.json`](/api/openapi.json) — feed it to any
OpenAPI-aware tool, or open it in
[editor.swagger.io](https://editor.swagger.io/).

</noscript>
