# $1: 0 on erase, 1 or more on upgrade.
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || :
fi
if [ "$1" -eq 0 ]; then
    rmdir /etc/byssus/conf.d /etc/byssus 2>/dev/null || :
fi
