# Security policy

## Supported versions

Security fixes are released for the latest minor version only.

| Version | Supported |
|---|---|
| 0.4.x | yes, once released |
| 0.3.x and earlier | no: affected by the vulnerabilities fixed in 0.4.0; upgrade |

The vulnerabilities fixed in 0.4.0, with mitigations for 0.3.2, are listed in
the [0.4.0 release notes](docs/RELEASE-NOTES-0.4.0.md#2-security-fixes).

## Reporting a vulnerability

Report privately through
[GitHub's private vulnerability reporting](https://github.com/akosidencio/tanod/security/advisories/new).
Do not open a public issue, pull request or discussion for a suspected
vulnerability.

Include what you can of:

- the Tanod version (`tanod version`) and how it is deployed;
- a redacted configuration, especially the routes, `cache` and `coalesce`
  settings and `server.trusted_proxies`;
- the requests that reproduce the issue and what you observed;
- the origin framework and version, if it matters.

Keep credentials, session cookies and other users' data out of the report.

## What happens next

The maintainer confirms the report, works on a fix in private where the
details are not already public, and publishes a GitHub security advisory with
the release that fixes it. Reporters are credited in the advisory unless they
ask not to be.

## Scope

In scope: the `tanod` binary and image, and the `@tanod/next` and
`@tanod/linux-x64` packages. The [threat model](docs/THREAT-MODEL.md) and
[security model](docs/SECURITY-MODEL.md) describe what Tanod defends against
and what it assumes. Notably out of scope: an origin that personalises a
response on a route the operator declared public, and HTTP parsing defects in
Pingora itself (report those to the Pingora project).
