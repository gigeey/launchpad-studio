# Security Policy

## Reporting a vulnerability

**Please do not open a public issue for a security vulnerability.**

Report it through GitHub's private vulnerability reporting:

1. Go to the [Security tab](https://github.com/gigeey/launchpad-studio/security)
   of this repository.
2. Click **Report a vulnerability**.
3. Describe the issue, the impact you believe it has, and the steps to
   reproduce it.

This opens a private advisory visible only to you and the maintainers. It is not
a public issue and does not appear anywhere until we publish it together.

You will need a GitHub account. We do not currently publish a security email
address; adding one that immediately gets scraped is worse than not having one.
If you cannot use GitHub for any reason, open a normal issue saying only that
you need a private channel — with no detail about the vulnerability itself — and
we will arrange one.

### What to expect

This project is maintained by one person. Setting expectations honestly rather
than quoting a service level we cannot meet:

- **Acknowledgement:** we aim for within 5 working days.
- **Assessment:** we will tell you whether we consider it a vulnerability, and
  why, once we have looked properly.
- **Fix:** timing depends on severity and complexity. We will keep you updated
  in the advisory thread rather than going quiet.
- **Credit:** we will credit you in the published advisory unless you would
  rather we did not. Please say which you prefer.

We do not operate a bug bounty and cannot offer payment.

### Disclosure

We will work with you on a disclosure timeline. Our default is to publish the
advisory once a fix is released. If we cannot produce a fix in reasonable time,
we would rather publish the advisory with a workaround than leave users unaware.

When an advisory is published, GitHub can assign a CVE and notify downstream
users through the GitHub Advisory Database.

## Supported versions

Launchpad Studio is pre-1.0 and moves quickly.

| Version | Supported |
|---|---|
| Latest release | Yes |
| Anything older | No |

Security fixes land in the next release. We do not backport to earlier versions.

## Scope

This policy covers the code in this repository.

Things that are **in scope** and worth reporting:

- Anything that lets an untrusted input — a file the agent reads, a tool result,
  a model response, or MCP server output — escape its intended boundary, execute
  commands, or read files it should not.
- Credential handling: provider API keys, OAuth tokens, and anything written to
  or read from the OS keychain.
- Anything that causes secrets to be written to logs, transcripts, or telemetry.

Things that are **out of scope**:

- Vulnerabilities in third-party dependencies with no exploitable path in this
  project. Report those upstream; tell us if we should pin or patch.
- The behaviour of third-party model providers or MCP servers you configure.
- Findings that require an attacker to already have local code execution as the
  user running the application.

## A known limitation, stated plainly

This application runs an AI agent that executes tools — including shell commands
and file writes — on your machine. That is its purpose, not a defect. The
security boundary is the permission system that gates those tools, and the
directory the agent is scoped to.

If you are evaluating this project for a security-sensitive environment, read
`guide/DEVELOPING.md` and treat the agent as software running with your user's
privileges, because that is what it is.

## Maintainer note

Private vulnerability reporting must be **enabled** on the repository for the
"Report a vulnerability" button to appear
(Settings → Advanced Security → Private vulnerability reporting). It is off by
default. If it is disabled, the instructions at the top of this file are wrong
and reporters have no private channel.
