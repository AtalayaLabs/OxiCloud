# Security Policy

## Reporting a Vulnerability

Please report security issues through **GitHub Security Advisories** — the
private-disclosure channel integrated with this repository:

  <https://github.com/EdouardVanbelle/OxiCloud/security/advisories/new>

If GitHub isn't a viable channel for you (organisational policy, no GH
account, etc.), email a maintainer directly at
<opensource+security@edouard.vanbelle.fr>. Please include the word `security`
in the subject line so it routes ahead of general project mail.

**Please do NOT open public GitHub issues for security vulnerabilities.**
A public issue makes the finding available to attackers before the fix
ships, which is exactly what we're trying to avoid.

## What to include

A short, specific report is far more useful than a long generic one. If
you can share:

- A concise description of the issue and its impact
- Steps to reproduce, or a proof-of-concept if one exists
- Affected component (REST API, WebDAV, NextCloud DAV, CalDAV, CardDAV,
  WOPI, auth, frontend, …)
- Affected version or commit hash
- Any remediation you've already identified

Both executed exploits AND code-review findings are welcome — mention
which one it is (e.g. "I found this pattern in the source but couldn't
build the service to verify runtime") so we know what to expect.

If you're not sure whether something is a vulnerability, err on the
side of reporting; we'd rather triage a false positive than miss a
real issue.

## Response expectations

OxiCloud is maintained by a small volunteer team. Realistic timelines:

- **Acknowledgement:** within 5 business days
- **Initial triage and severity assessment:** within 14 days
- **Fix on `main`:** timeline depends on complexity, communicated during
  triage
- **Public disclosure:** coordinated with the reporter, typically after
  the fix has been available on `main` long enough for downstream users
  to update

If you don't hear back within 5 business days, please ping the private
advisory or resend the email — reports occasionally get missed.

## Scope

**In scope:**

- Server (`src/`) — anything reachable via the HTTP surface
  (REST API, WebDAV, NextCloud DAV, CalDAV, CardDAV, WOPI) or via
  authentication / authorization / session management
- Frontend (`frontend/`) — client-side issues (XSS, CSRF gaps,
  insecure client-side storage, DOM sinks)
- Build artifacts and release tarballs — supply-chain and packaging
  integrity
- The migration and background-job surfaces — anything an authenticated
  user or admin can trigger

**Out of scope** (please don't spend your time on these):

- Missing security headers on non-authenticated public endpoints
  (already tracked)
- Rate-limit tuning suggestions
- Denial-of-service via resource exhaustion at scales beyond the
  documented deployment guidance
- Findings that require attacker-controlled physical or root access to
  the server host
- Vulnerabilities in third-party dependencies that don't have a
  reachable path from OxiCloud code (report those upstream)

## Safe-harbor

We won't pursue legal action against researchers acting in good faith
under this policy — that means:

- Not accessing or modifying data belonging to other users beyond the
  minimum needed to demonstrate the issue
- Not degrading service for others (no volumetric testing without
  coordination)
- Reporting the issue privately before any public disclosure
- Giving us reasonable time to fix before publishing

Testing against your own self-hosted instance is always fine. Testing
against a third-party OxiCloud deployment requires explicit permission
from that deployment's operator.

## Credit

We're happy to credit reporters in the fix commit, release notes, and
this file's history. Tell us your preferred name or handle when you
report, or say if you'd rather stay anonymous.

## Prior reports

Coordinated disclosures we've received and resolved:

- **2026-09-05** — Timing side-channel in WebDAV lock-token comparison
  (`evaluate_if_header` in `src/interfaces/api/handlers/webdav_handler.rs`
  used plain `==` on state-tokens, byte-wise with early exit). Fixed by
  routing lock-token comparisons through a `subtle::ConstantTimeEq`
  helper. Practical exploitability was marginal (ns-scale signal buried
  in ms-scale network jitter, ~5×10⁸ samples required within the lock's
  default 60 s–1 h lifetime), but the fix is small and matches the
  constant-time-compare hygiene applied elsewhere in the codebase.
  Reported by **Abdurazzoqov Javohir**
  ([@abdurazzoqovjavohir700-dev](https://github.com/abdurazzoqovjavohir700-dev))
  via responsible disclosure; fix landed in
  [PR #712](https://github.com/AtalayaLabs/OxiCloud/pull/712). Thanks
  for the clear report and the specific remediation suggestion.
