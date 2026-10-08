# Security policy

## Supported versions

arrow-kanban is pre-1.0. Only the **latest release** receives security fixes (currently
`v0.3.0`). Fixes land on `main` and ship in the next release. There are no backports to older
versions.

## Reporting a vulnerability

**Do not open a public issue.** Report it privately through GitHub's private vulnerability
reporting: go to the repository's **Security** tab and choose **Report a vulnerability**
(<https://github.com/Congruentsys/arrow-kanban/security/advisories/new>).

Please include the affected version or commit, a description of the impact, and steps or a
minimal reproduction.

## What to expect

- We aim to **acknowledge a report within 7 days**. This is a small project, and that is the
  number we can keep.
- We will keep you updated while we assess it and work on a fix, and agree a disclosure date with
  you. Our default is to publish an advisory when the fixed release ships, and we credit you
  unless you ask us not to.
- Please give us a reasonable chance to release a fix before disclosing publicly.

## Scope

In scope: defects in this repository's code that let an attacker do something the documented
contract forbids. Examples include corrupting or losing acknowledged writes, bypassing writer
fencing, reading or writing outside the board's data directory, or crashing the server with
crafted input.

Out of scope:

- **Network exposure of the NATS server.** arrow-kanban has no authentication or authorisation
  layer of its own. In multi-agent mode it trusts whatever can reach its NATS subjects. Exposing
  that NATS server to an untrusted network is a deployment decision, not a vulnerability in this
  code: secure it with NATS's own authentication, TLS and network controls.
- Anyone who can already write to the board's `.arrow-kanban/` directory can change the board.
  That is the storage model, not a bypass.
- Vulnerabilities in dependencies that are already public upstream. Report those to the
  dependency. Do tell us if arrow-kanban uses the dependency in a way that makes the issue
  exploitable here.
