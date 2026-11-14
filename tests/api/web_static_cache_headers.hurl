# =============================================================
# OxiCloud – response headers on the STATIC WEB layer
# (`interfaces/web/mod.rs`, not `/api`)
#
# Scope note, because it explains what is missing here rather than
# what is covered: this scenario only asserts things that hold with
# NO BUILT FRONTEND. The api-test runner has none — `static-dist/` is
# gitignored, CI's `build` job uploads only `target/release/oxicloud`,
# and that binary is built `--features plugins`, not `bundled-assets`.
# So the SPA shell is absent in CI and every shell-dependent
# assertion (an unmatched client route answering 200 + text/html +
# `Cache-Control: no-store`, `GET /robots.txt` serving the file) would
# be green on a dev box and red in CI. Those live in the Playwright
# job (`tests/e2e/spa/not-found.spec.ts`), which builds the SPA first
# and is the only harness where the static layer is populated.
#
# What survives without a frontend is a MISS, and a miss is exactly
# the regression below.
#
# ─────────────────────────────────────────────────────────────
# Regression pinned: a 404 under `/_app/immutable/` used to be
# answered `Cache-Control: public, max-age=31536000, immutable`.
#
# The prefix wore a `SetResponseHeaderLayer::overriding`, which is
# status-blind — it stamps the year-long immutable value on whatever
# the inner service returned, hit or miss. SvelteKit content-hashes
# these filenames so a HIT genuinely can never change, but a MISS
# then told the browser to cache an absence for a year. A client that
# requested an asset mid-deploy, before the file was on disk, pinned
# that 404 with no way to revalidate: the app stayed broken in that
# one browser profile until the user cleared their cache by hand, and
# nothing server-side could reach in and fix it.
#
# Found by reading a 404's headers, never by a test. Note the layer
# this test defends: `immutable_cache_headers` has its own unit test
# in `interfaces/web/mod.rs` covering both branches of the status
# check, and that test CANNOT catch this — swap the fn back for a
# status-blind layer at any of its three registration sites (one
# filesystem, two embedded) and the unit test still passes because
# the fn itself is untouched. Only a request through the real router
# proves the fn is still wired to the route. That is why this pin is
# at the wire and not only in Rust.
#
# `X-Robots-Tag` rides along in the same assertion block. It is the
# only CI-reachable coverage for it — the header is applied to the
# whole web router, so even this 404 carries it, while every other
# web response needs the shell that CI lacks.
#
# Anonymous on purpose: the static layer is public (the SPA has to
# load before anyone can log in), so a request here needs no token
# and the scenario leaves no DB state to tear down.
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/web_static_cache_headers.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 – A miss under the immutable prefix.
#
# The filename is deliberately shaped like a real SvelteKit asset
# (`<name>.<hash>.js`) so the request exercises the same route match
# a genuine stale reference would, rather than something a future
# path filter might reject early for looking bogus.
#
# 404 is the honest answer and is NOT what regressed — the status was
# always right. The header is the assertion.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/_app/immutable/chunks/does-not-exist.aaaaaaaa.js

HTTP 404
[Asserts]
header "Cache-Control" == "no-cache, no-store"
header "X-Robots-Tag" == "noindex, nofollow"


# ─────────────────────────────────────────────────────────────
# Step 2 – Same for a miss directly under the prefix root, not in a
# subdirectory. The filesystem path serves this prefix with a
# `ServeDir` and the embedded path with a `{*path}` wildcard route;
# a one-segment path and a two-segment path take visibly different
# code in both, so pin the pair rather than trusting that one
# generalises.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/_app/immutable/does-not-exist.aaaaaaaa.js

HTTP 404
[Asserts]
header "Cache-Control" == "no-cache, no-store"

# No traversal step here on purpose. `GET /_app/immutable/../../Cargo.toml`
# looks like the obvious third case, but curl (and so hurl) normalises
# `..` out of the path before it leaves the client, so the request that
# reaches the server is `GET /Cargo.toml` — the SPA fallback, never the
# immutable prefix. The test would pass while asserting nothing about the
# code its comment named. Traversal belongs where the normalisation can
# be bypassed (a raw socket or an encoded-segment fuzz case), not here.
