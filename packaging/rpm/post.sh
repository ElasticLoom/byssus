# $1: 1 on install, 2 or more on upgrade.
if command -v systemd-sysusers >/dev/null 2>&1; then
    systemd-sysusers byssus.conf || :
elif ! getent passwd byssus >/dev/null 2>&1; then
    useradd --system --user-group --no-create-home --home-dir /nonexistent \
        --shell /sbin/nologin --comment "Byssus filesystem attachment daemon" byssus || :
fi
install -d -o root -g root -m 0755 /etc/byssus /etc/byssus/conf.d
# State directory, so `byssus dry-run` and `byssus check` work before the
# first start (the unit's StateDirectory= would otherwise create it then).
install -d -o byssus -g byssus -m 0750 /var/lib/byssus
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || :
    # On upgrade, restart a running daemon so it uses the new binary.
    # byssusd never unmounts on exit, so consumers are not disturbed.
    if [ "$1" -ge 2 ]; then
        systemctl try-restart byssusd.service >/dev/null 2>&1 || :
    fi
fi
