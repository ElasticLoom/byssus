# $1: 0 on erase, 1 or more on upgrade.
if [ "$1" -eq 0 ] && [ -d /run/systemd/system ]; then
    # Stopping does not remove mounts; see "Removing Byssus" in
    # /usr/share/doc/byssus/OPERATIONS.md.
    systemctl disable --now byssusd.service >/dev/null 2>&1 || :
fi
