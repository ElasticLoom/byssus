# Security Policy

Byssus runs with `CAP_SYS_ADMIN` and mediates filesystem exposure between
isolated workspaces, so we take security reports seriously.

## Reporting a vulnerability

**Please do not open a public issue for security vulnerabilities.**

Report vulnerabilities privately through GitHub's private vulnerability
reporting:

1. Go to the [Security tab](https://github.com/ElasticLoom/byssus/security) of
   this repository.
2. Click **Report a vulnerability**.

Please include:

- a description of the issue and its impact;
- the affected version or commit;
- steps to reproduce, or a proof of concept;
- your kernel version and deployment method (systemd, `setcap`, root), if
  relevant.

We aim to acknowledge reports within 7 days and to agree on a disclosure
timeline with the reporter. We are happy to credit reporters in the advisory
unless you prefer to remain anonymous.

## Supported versions

Byssus is in early development and has not yet had a release. Once releases
begin, security fixes will be made to the latest release.

## Scope

The [security contract](docs/DESIGN.md#security-contract) describes the
guarantees Byssus intends to provide. Violations of any of them are in scope,
for example:

- exposing files outside a configured source, or at a location outside a
  configured target root;
- producing a writable, setuid-capable, device-capable or (when configured)
  executable view;
- exposing nested mounts from within a source;
- unmounting or modifying a mount Byssus did not create;
- retaining capabilities or privileges beyond those documented.

Out of scope: consequences of deliberately insecure deployment, such as
granting untrusted users write access to membership directories or
configuration.
