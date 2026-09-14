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

Security fixes are made to the most recent release.

## Trust boundaries

Byssus involves these parties, from most to least trusted:

| Party | Trusted to | Must not be able to |
|-------|------------|---------------------|
| **Operator** (root) | write configuration, start the daemon, manage mounts | — |
| **`byssusd`** (service user with only `CAP_SYS_ADMIN`) | create and remove the mounts its configuration describes | act on anything its configuration and the parties below it do not direct |
| **`byssus` group members** (operators and monitoring tools) | read the state file and run `byssus status`, which shows every group and member name | change state, configuration, membership or mounts |
| **Membership writer** (the application) | change the exposure of groups whose membership directory it can write | expose anything outside those groups' configured sources, affect other groups, place views outside the target root, or influence configuration or the daemon's privileges |
| **Source owners** (users who write inside source directories) | put arbitrary files, symlinks and special files in their own source | make a view carry setuid, device or (when configured) write or exec capability, or expose mounts nested inside the source |
| **Consumers** (containers or workspaces that see views) | read (or write, for `read_only = false` groups) the views they are given | change membership, affect host mounts, or reach anything beyond their views |
| **Other local users** | nothing | influence Byssus at all |

A report is most interesting when a party can do something its row says it
must not. `CAP_SYS_ADMIN` is powerful: a compromised daemon could misuse
mounts to gain much more, which is why a lower-trust party steering the
daemon into doing so is in scope.

## Scope

The [security contract](docs/DESIGN.md#security-contract) describes the
guarantees Byssus intends to provide. Violations of any of them are in scope.
Classes of issue we are especially interested in:

- **Confinement escapes:** a member name, template or directory layout that
  exposes files outside a configured source, or places a view outside its
  target root or in another group's space.
- **Races an unprivileged party can win:** renames, symlink swaps or directory
  replacement during membership scanning, source resolution, mount creation or
  unmounting.
- **Weakened views:** a view that is ever setuid- or device-capable, or
  writable, executable or symlink-following when configured otherwise, or that
  exposes nested mounts.
- **Mount ownership confusion:** unmounting, replacing or modifying a mount
  Byssus did not create.
- **Propagation leaks:** mounts made by a consumer reaching the host, or views
  reaching places they were not configured for.
- **Privilege normalization flaws:** capabilities, user or group IDs,
  securebits or `no_new_privs` left other than documented.
- **Configuration and state trust:** loading configuration that fails the
  ownership checks, state that makes Byssus act on mounts it does not own, or
  state readable outside the `byssus` group.
- **Memory safety** in the small `unsafe` system call layer.
- **Denial of service across a boundary:** a membership writer, source owner
  or consumer crashing or hanging the daemon, or stopping other groups from
  being reconciled.

Out of scope:

- Actions that require root, `CAP_SYS_ADMIN` in the host mount namespace, or
  write access to configuration or the state directory: those parties can
  already do anything Byssus can. Hardening suggestions in this area are
  welcome as regular issues.
- Consequences of a deliberately insecure deployment, such as giving consumers
  write access to membership directories, or using `read_only = false` groups
  with consumers who are not trusted to change every member.
- A membership writer disrupting only its own group.
- Kernel vulnerabilities, which should be reported upstream. Incorrect use of
  kernel interfaces by Byssus is in scope.
