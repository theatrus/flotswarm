#!/bin/sh
set -e

# Dedicated unprivileged service user (idempotent across deb/rpm).
if ! id flotswarm >/dev/null 2>&1; then
    useradd --system --no-create-home --shell /usr/sbin/nologin flotswarm 2>/dev/null \
        || adduser --system --no-create-home --shell /usr/sbin/nologin flotswarm 2>/dev/null \
        || true
fi

mkdir -p /etc/flotswarm/actions.d
chown flotswarm:flotswarm /etc/flotswarm /etc/flotswarm/actions.d 2>/dev/null || true

# agent.conf is NOT a packaged conffile — create from the example on first
# install only, so upgrades never clobber it. Mode 0600 (holds no secrets on EC2,
# but may hold AWS keys on off-EC2 hosts).
CONF=/etc/flotswarm/agent.conf
EXAMPLE=/usr/share/doc/flotswarm-agent/agent.conf.example
if [ ! -f "$CONF" ] && [ -f "$EXAMPLE" ]; then
    install -m 600 -o flotswarm -g flotswarm "$EXAMPLE" "$CONF"
    echo "Created $CONF from example."
fi

systemctl daemon-reload >/dev/null 2>&1 || true

cat <<'EOF'
flotswarm-agent installed.

Next steps:
  1. Edit /etc/flotswarm/agent.conf   (FLOTSWARM_HOST, FLOTSWARM_QUEUE_URL, AWS_REGION).
  2. Add allowlist actions to /etc/flotswarm/actions.d/*.toml
     (examples in /usr/share/doc/flotswarm-agent/actions.d/).
  3. Grant scoped sudo for those actions' run_as user
     (see /usr/share/doc/flotswarm-agent/sudoers.example).
  4. systemctl enable --now flotswarm-agent.service
EOF
