#!/bin/sh
set -e

# Leave /etc/flotswarm and the flotswarm user in place on removal (config +
# allowlist are operator data). Just refresh systemd.
systemctl daemon-reload >/dev/null 2>&1 || true
