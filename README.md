# Byssus

[![CI](https://github.com/ElasticLoom/byssus/actions/workflows/ci.yml/badge.svg)](https://github.com/ElasticLoom/byssus/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

> *Byssus creates live filesystem attachments between isolated workspaces.*

Byssus is a small Linux daemon that gives a group of isolated workspaces — for
example, one container per project — a live, **read-only** view of each
other's directories. Group membership is declared by creating or deleting empty
files; the daemon turns that into bind mounts, and mount propagation carries
them into already-running containers without restarts.

> **Status: pre-release.** The daemon and CLI are functional and tested, but
> Byssus has not had a release or an external security review yet. See
> [docs/TODO.md](docs/TODO.md) for remaining work.

## How it works

```
touch /srv/example/membership/research/libcurl
        │
        ▼
byssusd (inotify) ──► validates the name ──► read-only bind mount of
                                             /srv/example/projects/libcurl/workspace
                                             at /srv/example/groups/research/view/libcurl
        │
        ▼
containers that mount …/research/view with rslave propagation
see /group/libcurl immediately
```

Removing the file removes the mount.

Key properties:

- **No copies or sync** — consumers read the source directory itself.
- **Read-only, nosuid, nodev, noexec** views, applied atomically before a mount
  becomes visible.
- **Confined path resolution** — every path is resolved beneath trusted root
  directories with `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`.
- **Never touches mounts it did not create** — ownership is verified by kernel
  mount identity.
- **Least privilege** — runs as an unprivileged user holding only
  `CAP_SYS_ADMIN`, with no ability to bypass file permissions.
- **Nested mounts are never exposed** from inside a source directory.
- **Mounts survive daemon restarts**; the daemon reconciles on startup.

## Requirements

- Linux 5.12 or newer
- Rust 1.85 or newer to build

## Binaries

| Binary | Purpose | Privileges |
|--------|---------|------------|
| `byssusd` | Daemon: watches membership directories and reconciles mounts | `CAP_SYS_ADMIN` |
| `byssus` | CLI: `status`, `dry-run`, `reconcile`, `version` | none, except `reconcile` |

## Configuration example

```toml
# /etc/byssus/byssus.toml
[daemon]
user = "byssus"

[groups.research]
source_root = "/srv/example/projects"
source      = "{name}/workspace"
target_root = "/srv/example/groups/research/view"
target      = "{name}"
membership  = "/srv/example/membership/research"
```

For groups your application creates at runtime, a **group set** defines them
once: each directory beneath `membership_root` (here `<org>/<group>`) is a
group, so creating a group is `mkdir`, with no configuration change or root.

```toml
[group_sets.projects]
membership_root = "/srv/example/membership/projects"
source_root     = "/srv/example/orgs"
source          = "{group}/projects/{name}/workspace"
target_root     = "/srv/example/orgs"
target          = "{group}/groups/{subgroup}/view/{name}"
```

See [INTEGRATION.md](docs/INTEGRATION.md#groups-created-at-runtime-group-sets).

## Install

Byssus has no release yet; install from source (Linux 5.12+, Rust 1.85+):

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl
sudo install -m 0755 target/x86_64-unknown-linux-musl/release/byssusd /usr/local/bin/
sudo install -m 0755 target/x86_64-unknown-linux-musl/release/byssus  /usr/local/bin/
```

Releases will provide `.deb` and `.rpm` packages and static binary archives:

```bash
sudo apt install ./byssus_<version>-1_amd64.deb     # Debian, Ubuntu
sudo dnf install ./byssus-<version>-1.x86_64.rpm    # Fedora, RHEL
```

Packages install the binaries, systemd unit and service user but do not start
the daemon. Then set up source directories, the propagation anchor and group
configuration, check with `sudo byssus dry-run`, and start with
`sudo systemctl enable --now byssusd` — see the
[installation and operations guide](docs/OPERATIONS.md).

## Try it

On Linux 5.12+, without root:

```bash
scripts/playground.sh
```

This opens a shell in a throwaway namespace with a sample deployment, a
simulated container and `byssusd` running. Type `help` for things to try.

## Documentation

- [Installation and operations guide](docs/OPERATIONS.md) — install, configure, run, troubleshoot
- [Integration guide](docs/INTEGRATION.md) — containers and managing membership from an application
- [Reference](docs/REFERENCE.md) — configuration, naming rules, CLI, log format
- [Design and security contract](docs/DESIGN.md) and [design decisions](docs/DECISIONS.md)
- [Milestones and TODO](docs/TODO.md)
- [Security policy](SECURITY.md) · [Contributing](CONTRIBUTING.md) · [Releasing](RELEASING.md)

## Background

Byssus was built for [ElasticLoom](https://github.com/ElasticLoom), where it
lets research projects read each other's findings. It has no dependency on
ElasticLoom and is intended to be useful to any system that needs live,
read-only directory sharing across container boundaries.

The name comes from the byssus threads mussels use to attach themselves to
surfaces.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you shall be licensed as above, without any
additional terms or conditions.
