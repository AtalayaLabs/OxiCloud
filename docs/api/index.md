# API Reference

OxiCloud exposes two machine-readable schemas that describe its
programmable surface:

- **[REST API](./rest)** — the HTTP endpoints under `/api/*`,
  documented as an [OpenAPI 3.1](https://spec.openapis.org/oas/latest.html)
  specification. Rendered here with [Scalar](https://scalar.com).
  Everything the SPA calls goes through this surface, plus the
  admin management endpoints, sharing / grants, chunked uploads,
  and every other REST-shaped feature.

- **[Message Bus](./bus)** — the WebSocket real-time event stream,
  documented as an [AsyncAPI 3](https://www.asyncapi.com/) specification.
  Covers the topics an authenticated client can subscribe to
  (`user`, `admin`, `folder:{id}`, `share:{token}`, …) and the
  payload shapes servers push down. Rendered here with the official
  AsyncAPI HTML template.

## Which one am I looking at?

- If you're integrating a REST client (curl, Python, Go, another
  server), you want **REST**.
- If you're building a client that reacts to live events (new
  files, share creation, job progress, notifications), you want
  **Message Bus** in addition to REST.
- If you're building a NextCloud-compatible or WebDAV / CalDAV /
  CardDAV client, those protocols are described by their
  respective RFCs — this API reference is for OxiCloud's native
  surfaces only.

## Downloading the raw specs

Both specs are also served as raw JSON alongside the rendered
viewers, in case you want to plug them into your own tooling
(Postman, `openapi-generator`, an IDE, etc.):

- [`openapi.json`](/api/openapi.json)
- [`asyncapi.json`](/api/asyncapi.json)

The pages linked above load these same URLs. Under the hood the
specs are generated from the Rust source at
`resources/gen/openapi.json` and `resources/gen/asyncapi.json`;
they refresh whenever the backend's `#[utoipa::path]` and
AsyncAPI-annotated types change.
