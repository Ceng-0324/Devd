# Security policy

## Supported versions

Only the current `main` branch and the latest GitHub prerelease are in scope for
security fixes. devd is a local development tool; it does not claim to be a
multi-user sandbox. Commands in a configuration or recovery recipe run with the
launching user's operating-system permissions.

## Reporting a vulnerability

Use this repository's [private vulnerability reporting form](https://github.com/Ceng-0324/Devd/security/advisories/new).
Do not put exploit details, secrets, production paths, or proof-of-concept code
in a public issue. Include the devd version or commit, platform, configuration
shape with sensitive values removed, reproduction steps, impact, and any
mitigation already applied.

We will acknowledge the report, investigate it privately, coordinate a fix when
needed, and credit the reporter only with permission. Public disclosure timing
depends on the affected behavior and a patched release; there is no fixed SLA
for this alpha project. If the private form is unavailable, open a minimal
public issue requesting a private contact path and omit all vulnerability
details.

## Scope notes

High-value reports include unauthorized process control, ownership or cleanup
escapes, stale-run control, report parser resource exhaustion, credential or
environment disclosure, and archive provenance bypasses. Third-party dependency
advisories are useful when they affect devd's supported builds; include the
advisory identifier and locked version.
