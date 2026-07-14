# Security Policy

## Reporting a vulnerability

**Please do not report security vulnerabilities through public issues, pull
requests, or discussions.**

Instead, report privately through GitHub's
[private vulnerability reporting](https://github.com/GetBusbar/headroom-hook/security/advisories/new)
(the **Security** tab on the repository). Please include a description of the
issue and its impact, the steps to reproduce, and any relevant configuration.

## Scope

`headroom-hook` is a rewrite gate that runs on Busbar's normalized request
path. It never holds provider credentials and never terminates client auth —
Busbar does. The security-relevant surface here is:

- The hook can only ever REWRITE or ABSTAIN. Busbar is fail-closed on the
  hook's side: a malformed, oversized, or slow reply means the original request
  proceeds unmodified. A bug in the hook can degrade compression; it cannot
  corrupt a request or leak upstream credentials it never sees.
- Inbound wire lines are bounded (8 MiB) and replies are capped (64 KiB), so a
  hostile or desynced peer cannot drive unbounded allocation.

## Supported versions

Fixes are applied to the latest `main` and released from there. The hook pins
the `headroom-core` revision it was verified against; dependency-security bumps
re-run the full test suite before release.
