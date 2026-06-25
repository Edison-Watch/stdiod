# Security Policy

## Supported versions

stdiod is pre-1.0, experimental software. Only the latest `main` / most recent
release receives security fixes. Pin a specific commit if you need stability.

| Version | Supported |
| --- | --- |
| latest `main` | ✅ |
| older commits | ❌ |

## Reporting a vulnerability

**Please do not report security issues through public GitHub issues, pull
requests, or discussions.**

Instead, report privately through either channel:

- **GitHub private advisory** - open the repository's **Security** tab and click
  **"Report a vulnerability"** ([private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)).
- **Email** - <security@edison.watch>.

Please include enough detail to reproduce: affected version/commit, platform,
configuration, and a description of the impact. We aim to acknowledge reports
within a few business days and will keep you updated on remediation. We support
coordinated disclosure and are happy to credit reporters.

## Security model and notes

stdiod runs as a long-lived daemon on a user's machine and handles credentials,
so a few properties are worth understanding:

- **Credentials at rest.** The API key (and optional secret key) are written in
  plaintext to `~/.config/edison-stdiod/config.toml` with file mode `0600`. They
  are not encrypted on disk. Protect the host account accordingly; rotate a key
  by re-running `edison-stdiod login --api-key …`, and remove all persisted state
  with `edison-stdiod uninstall --purge`.
- **Outbound-only transport.** The daemon makes a single outbound TLS WebSocket
  connection to the configured backend and authenticates with a Bearer token. It
  opens no inbound listening ports. Always use an `https://`/`wss://` backend URL
  outside of local development.
- **Child processes.** The daemon spawns local MCP server subprocesses as
  instructed by the authenticated backend. Those processes run with the
  privileges of the user running the daemon and can access that user's files and
  environment. Only connect a device to a backend you trust, and only register
  server commands you trust.
- **No independent audit.** This code has not undergone an external security
  review. Treat it as experimental.

If you are unsure whether something is a security issue, err on the side of
reporting it privately.
