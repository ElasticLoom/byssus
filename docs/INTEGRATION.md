# Integrating with Byssus

For platforms that build on Byssus: attaching containers to group views and
managing membership from an application. Installation and operation are
covered in [OPERATIONS.md](OPERATIONS.md).

## Contents

- [Attach containers](#attach-containers)
- [Integrate an application](#integrate-an-application)

---

## Attach containers

Mount the group's view into containers **read-only with `rslave`
propagation**. New members then appear without restarting the container.

Docker:

```bash
docker run \
  --mount type=bind,source=/srv/example/groups/research/view,target=/group,readonly,bind-propagation=rslave \
  ...
```

Compose:

```yaml
services:
  agent:
    volumes:
      - type: bind
        source: /srv/example/groups/research/view
        target: /group
        read_only: true
        bind:
          propagation: rslave
```

Inside the container, `/group/<member>` is each member's source directory,
read-only, with `nosuid`, `nodev` and (by default) `noexec`. Filesystems
mounted *inside* a member's source directory are never exposed.

## Integrate an application

Membership is file presence:

```bash
# Add a member
touch /srv/example/membership/research/libcurl
# Remove a member
rm /srv/example/membership/research/libcurl
```

Rules:

- Names use only `A–Z a–z 0–9 . _ -`, are at most 255 bytes, and must not
  start with `.`.
- A member file must be an **empty regular file**. Symlinks, directories and
  non-empty files are rejected (and a member whose file becomes non-empty is
  removed).
- Files whose names start with `.` are ignored without a warning, so an
  application can prepare a file under a hidden name and `rename(2)` it into
  place, and files like `.gitkeep` are harmless.
- Creating a membership file never fails because of Byssus: the daemon picks
  changes up asynchronously. Check `byssus status` (or its JSON output) to see
  whether an entry was accepted or rejected, and why.
- Changes take effect about 150 ms after the last change in a burst.
- Adding a member before its source directory exists is fine: it is mounted
  at the next periodic resync after the source appears (default 60 s), or
  immediately on any other membership change.

Write access to a membership directory is the authority to change what the
group sees. Restrict it to the application that manages membership.

To add or remove a whole group, change the configuration and reload:

```bash
systemctl reload byssusd      # sends SIGHUP
```
