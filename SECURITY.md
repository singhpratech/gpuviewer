# Security Policy

## Supported versions

gpuviewer is pre-release. Only the latest `0.1.x` on `main` receives fixes — there are no
backport branches.

## Reporting a vulnerability

Please report vulnerabilities **privately** via GitHub's "Report a vulnerability" form:

https://github.com/singhpratech/gpuviewer/security/advisories/new

Do not open a public issue for anything security-sensitive. This is a solo-maintainer
project, so response is best-effort — expect an acknowledgment within a few days and an
honest assessment of severity and timeline after that.

## Scope

gpuviewer is one unprivileged local binary. It reads GPU telemetry (sysfs/hwmon/fdinfo,
NVML via dlopen) and writes history to a local SQLite database. It runs no network
services, opens no sockets, and phones nothing home.

Things in scope: parsing bugs in untrusted-ish inputs (sysfs/fdinfo contents, `.gpvr`
files opened with `view`), the SQLite history layer, and anything that escalates beyond
the invoking user's privileges.

One deliberate non-bug: the `--on-event 'CMD'` hook executes a user-supplied command by
design. Running a command you typed is the feature, not a vulnerability — reports about
`--on-event` are only in scope if it executes something the user did *not* supply.
