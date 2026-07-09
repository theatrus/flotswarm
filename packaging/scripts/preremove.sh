#!/bin/sh
set -e

# Stop + disable on real removal (deb: "remove"/"purge"; rpm: final removal = 0).
case "$1" in
    remove | purge | 0)
        systemctl disable --now flotswarm-agent.service >/dev/null 2>&1 || true
        ;;
esac
