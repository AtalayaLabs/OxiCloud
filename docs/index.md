---
layout: home

hero:
  name: "OxiCloud"
  text: "Self-hosted cloud storage"
  tagline: "Files, calendar & contacts — blazingly fast, written in Rust"
  image:
    src: /logo.svg
    alt: OxiCloud Logo
  actions:
    - theme: brand
      text: Get Started
      link: /guide/installation
    - theme: alt
      text: View on GitHub
      link: https://github.com/DioCrafts/OxiCloud
    - theme: alt
      text: Why OxiCloud?
      link: /guide/

features:
  - icon: 🚀
    title: Blazingly Fast
    details: Single Rust binary, ~40 MB Docker image, <1s cold start, 30–50 MB idle RAM.
  - icon: 📁
    title: Full File Management
    details: Chunked uploads, BLAKE3 deduplication, trash, favourites, full-text search, thumbnails.
  - icon: 🔗
    title: WebDAV / CalDAV / CardDAV
    details: RFC-compliant protocols for files, calendars, and contacts. Works with all major clients.
  - icon: 📝
    title: Office Editing (WOPI)
    details: Edit documents in Collabora Online or OnlyOffice directly in the browser.
  - icon: 🔐
    title: Security First
    details: JWT + DPoP + Opaque (Argon2id), OIDC/SSO (Keycloak, Authentik, Azure AD), role-based access, shared links.
  - icon: 🌍
    title: 14 Languages
    details: EN, ES, DE, FR, IT, PT, NL, ZH, JA, KO, AR, HI, FA, RU — and growing.
---
