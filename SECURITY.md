# Security policy

Needle is a pre-alpha developer project. No version is supported for
production, and this policy does not create a supported release or a
compatibility window.

This policy covers reports about the current default branch. Security response
and remediation are best-effort; there is no service-level agreement or
guaranteed response or fix timeline. The project has not completed a
third-party security audit.

## Reporting a vulnerability

Do not disclose vulnerability details in a public issue, pull request,
discussion, or other public channel.

When GitHub private vulnerability reporting is available for this repository,
please use it to submit the report. If the **Report a vulnerability** button is
not available, use this safe fallback:

1. Open a public issue with no vulnerability details and ask the maintainers
   for a private contact channel.
2. Wait for the maintainers to respond before sharing any details.
3. Send the details only through the private channel the maintainers provide.

Keep exploit steps, proof-of-concept code, affected paths, credentials, and
other sensitive information out of the public issue.

For ordinary bugs, regressions, and feature requests, use the normal public
issue or pull-request process instead of this security-reporting route.

## Known dependency advisories

The installed `react-router-dom` version is `7.18.1`, which falls within the
affected range (`>=7.12.0, <8.3.0`) of
[GHSA-qwww-vcr4-c8h2](https://github.com/advisories/GHSA-qwww-vcr4-c8h2).
Upstream describes this advisory as affecting unstable React Server Component
(RSC) APIs. Needle's current Vite client-side SPA uses `BrowserRouter` and
`createRoot` and does not enable those RSC APIs; this bounded non-exposure does
not mean the dependency is generally safe.

Upstream identifies `8.3.0` as the first patched release, but that release is
not available from npm as of 2026-08-03. This is a temporary, scoped exception,
not evidence of a completed security audit. Do not enable the affected RSC APIs;
monitor for a published patched release, then upgrade and rerun `npm audit`,
tests, lint, and build. Remove this exception once that validation succeeds.
