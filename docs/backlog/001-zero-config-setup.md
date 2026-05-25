# Priority 1: Unified Zero-Config Account Setup (The Happy Path)

**Business Value:** This is the core table-stakes feature for competing with major cloud providers. By removing the need to hunt down complex URLs, we drastically reduce onboarding friction and support tickets for self-hosters.

**User Story:**
As a privacy-conscious self-hoster, I want to connect my calendar and contacts apps (like Thunderbird, Apple Calendar, and DAVx5) using only my OxiCloud domain and login details so that I can seamlessly sync my data without needing to understand or copy-paste complex server URLs.

**Acceptance Criteria:**

**Scenario 1: Client Auto-Discovery Redirects**
*Given* an active OxiCloud instance running at a standard domain (e.g., `cloud.example.com`)
*When* an unauthenticated or authenticated client makes an HTTP GET request to `/.well-known/caldav` or `/.well-known/carddav`
*Then* the server MUST return an HTTP 301 or 302 redirect pointing to the correct OxiCloud DAV root or principal URL.

**Scenario 2: Server Capabilities Verification**
*Given* a sync client has discovered the DAV root URL
*When* the client sends an HTTP `OPTIONS` request to the DAV root
*Then* the server MUST return a 200 OK
*And* the response headers MUST include standard DAV capabilities (e.g., `DAV: 1, 3, calendar-access, addressbook`).

**Scenario 3: Principal Home-Set Discovery**
*Given* an authenticated sync client with valid user credentials
*When* the client sends an HTTP `PROPFIND` request to the principal URL looking for home sets
*Then* the server MUST return a `207 Multi-Status` response
*And* the XML body MUST correctly map the user to their specific `calendar-home-set` and `addressbook-home-set` URLs.

**Scenario 4: E2E Integration Success**
*Given* our automated E2E test suite simulating DAVx5 and Apple Calendar auto-discovery flows
*When* the test runner provides a valid domain, username, and password
*Then* the suite MUST successfully discover the calendars and address books without requiring manual URL overrides
*And* the test MUST pass in the CI/CD pipeline.

#### 🛡️ Security Constraints (Security Reviewer)
*   **Redirect Scheme:** The `.well-known` redirects MUST enforce `https://` schemes unless explicitly running in a local development environment.
*   **XXE Prevention:** All incoming `PROPFIND` requests containing XML bodies MUST be parsed with a securely configured XML parser that completely disables DTDs (Document Type Definitions) and external entity resolution to prevent XXE (XML External Entity) attacks.
*   **Rate Limiting:** The `.well-known` endpoints and DAV root `OPTIONS` must have IP-based rate limiting to prevent reconnaissance/DDoS.

#### 🦀 Architectural Constraints (Codebase/Rust Expert)
*   **Routing:** In Axum 0.8.9, use `axum::routing::get` for the `.well-known` paths. Return `axum::response::Redirect::permanent()` (301) for discovery URLs.
*   **XML Parsing/Serialization:** Use `quick-xml` for XML generation and parsing. It is memory-safe, fast, and plays nicely with Tokio's async model without blocking the reactor. Do *not* use heavy DOM-based parsers.
*   **Layer Isolation:** Keep DAV HTTP delivery isolated. Create an `infrastructure/delivery/http/dav` module. Axum extractors should call traits defined in the `application` layer to fetch the principal home-sets, keeping the HTTP layer decoupled from the SQLx queries.
*   **Database:** Access the PostgreSQL pool via Axum's `State` extractor (`axum::extract::State<Arc<AppState>>`). Use SQLx 0.8.6 macros (`query!`) to securely fetch the principal paths based on the authenticated user ID.

#### 🧠 Tech Lead Risk Synthesis & Notes
*   **Risk - strict XML parsing by clients:** CalDAV/CardDAV clients (especially Apple Calendar) are notoriously strict and will silently fail if namespaces or XML tags are even slightly off in the `207 Multi-Status` response.
    *   *Mitigation:* Start with a minimal viable XML response. Build a strict suite of unit tests around the `quick-xml` serialization comparing our output exactly to RFC specifications.
*   **Risk - HTTP Method Support:** Axum's default router doesn't explicitly expose `PROPFIND` as a standard builder method like `.get()` or `.post()`.
    *   *Mitigation:* Use `axum::routing::method_routing::on` with `axum::http::Method::from_bytes(b"PROPFIND").unwrap()`. We will need to define custom HTTP method constants for WebDAV (`PROPFIND`, `PROPPATCH`, `REPORT`, etc.).
