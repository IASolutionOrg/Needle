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

## Resolved dependency advisories

[GHSA-qwww-vcr4-c8h2](https://github.com/advisories/GHSA-qwww-vcr4-c8h2)
was remediated by migrating the Vite client-side SPA from the removed
`react-router-dom` package to `react-router` `8.3.0`. Needle does not enable the
affected unstable React Server Component (RSC) APIs. This remediation does not
constitute a completed third-party security audit.
