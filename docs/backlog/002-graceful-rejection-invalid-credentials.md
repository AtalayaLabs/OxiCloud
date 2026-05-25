# Priority 2: Graceful Rejection on Invalid Credentials

**Business Value:** Security and User Feedback. When auto-discovery fails, clients like GNOME Calendar or Apple Calendar often throw cryptic XML parsing errors if the server returns a 200 OK with an HTML login page instead of a proper DAV error. We need to fail gracefully so the user knows they just typed their password wrong.

**User Story:**
As a privacy-conscious self-hoster, I want to be explicitly told if I entered the wrong password during the automated setup so that I can correct my mistake without thinking my OxiCloud server is broken.

**Acceptance Criteria:**

**Scenario 1: Invalid Credentials during PROPFIND**
*Given* a sync client is attempting to automatically discover calendars or address books
*When* the client sends a `PROPFIND` request to the principal URL with invalid or expired credentials
*Then* the server MUST immediately return an HTTP `401 Unauthorized` status code
*And* the server MUST NOT return a 200 OK with an HTML login page or standard redirect (which breaks client parsers)
*And* the HTTP response MUST include the `WWW-Authenticate` header to trigger the client's native credential prompt.

**Scenario 2: Missing Credentials on protected endpoints**
*Given* a sync client attempts to query capabilities on a protected DAV path without an Authorization header
*When* the `OPTIONS` or `PROPFIND` request is received
*Then* the server MUST respond with an HTTP `401 Unauthorized` code.

#### 🛡️ Security Constraints (Security Reviewer)
*   **Timing Attacks:** The authentication validation MUST execute in constant time regardless of whether the username exists in the database. Use dummy password verification if the user is not found.
*   **WWW-Authenticate Header:** The response header must be strictly formatted: `WWW-Authenticate: Basic realm="OxiCloud"`.
*   **Brute Force Protection:** Emit an audit log event (e.g., `AuthFailed { ip, username }`) that external tools like Fail2Ban can parse, or implement a local token bucket rate limiter in the app layer for failed attempts.

#### 🦀 Architectural Constraints (Codebase/Rust Expert)
*   **Auth Middleware:** Use `axum::middleware::from_fn_with_state` to apply an authentication guard *exclusively* to the `/dav/*` router namespace. Do not mix DAV routing with the SPA web routing.
*   **Credentials Extraction:** Use `axum_extra::typed_header::TypedHeader<Authorization<Basic>>` to safely extract Basic Auth credentials.
*   **Error Handling:** Implement `axum::response::IntoResponse` for a custom `DavError` enum. If auth fails, short-circuit and return `(StatusCode::UNAUTHORIZED, [("WWW-Authenticate", "Basic realm=\"OxiCloud\"")], "Unauthorized")`.

#### 🧠 Tech Lead Risk Synthesis & Notes
*   **Risk - Database Hammering (The "Basic Auth Spam" problem):** CalDAV/CardDAV clients send Basic Auth credentials on *every single request*. Hitting the PostgreSQL DB with a password hashing algorithm (like Argon2) on every `PROPFIND` and `OPTIONS` request will bottleneck the async runtime and melt the CPU.
    *   *Mitigation:* We absolutely cannot run Argon2 on every HTTP request. We must implement an in-memory caching layer (e.g., using the `moka` crate) for successful Basic Auth validations, mapping a hash of the `Authorization` header to a short-lived session/user ID (e.g., TTL of 5-10 minutes).
*   **Risk - Axum Fallback Route Conflict:** If the DAV routes aren't matched perfectly, Axum might fall back to the frontend SPA handler (which returns `index.html` with a 200 OK).
    *   *Mitigation:* The `Router` must define a strict boundary for DAV. Create a nested router for `/dav` that has its own explicit `fallback` handler which returns a `404 Not Found` (or `401` if unauthenticated), ensuring an HTML response is *never* accidentally generated for a DAV path.
