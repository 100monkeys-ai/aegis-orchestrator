#!/bin/sh
set -eu

# Allow the unprivileged runtime user to access host Docker socket when mounted.
SOCK_GID="$(stat -c '%g' /var/run/docker.sock 2>/dev/null || true)"
if [ -n "$SOCK_GID" ]; then
  if ! getent group "$SOCK_GID" >/dev/null 2>&1; then
    groupadd -f -g "$SOCK_GID" hostdocker >/dev/null 2>&1 || true
  fi
  usermod -aG "$SOCK_GID" aegis >/dev/null 2>&1 || true
fi

if [ "$#" -eq 0 ]; then
  set -- /usr/local/bin/aegis-runtime --daemon
fi

# Drop root privileges and run runtime in foreground as PID 1 child under tini.
exec su -s /bin/sh aegis -c 'exec "$@"' -- "$@"
