#!/bin/sh
set -eu

# Allow the unprivileged runtime user to access host Docker socket when mounted.
if stat -c '%g' /dev/null >/dev/null 2>&1; then
  SOCK_GID="$(stat -c '%g' /var/run/docker.sock 2>/dev/null || true)"
elif stat -f '%g' /dev/null >/dev/null 2>&1; then
  SOCK_GID="$(stat -f '%g' /var/run/docker.sock 2>/dev/null || true)"
else
  SOCK_GID=""
fi
if [ -n "$SOCK_GID" ]; then
  if ! getent group "$SOCK_GID" >/dev/null 2>&1; then
    if ! groupadd -f -g "$SOCK_GID" hostdocker >/dev/null 2>&1; then
      # If group creation failed, re-check whether a group with this GID now exists.
      if ! getent group "$SOCK_GID" >/dev/null 2>&1; then
        echo "Failed to create group with GID $SOCK_GID for Docker socket access" >&2
        exit 1
      fi
    fi
  fi
  if ! usermod -aG "$SOCK_GID" aegis >/dev/null 2>&1; then
    echo "Warning: failed to add user 'aegis' to group GID ${SOCK_GID}; Docker socket access may not work." >&2
  fi
fi

if [ "$#" -eq 0 ]; then
  set -- /usr/local/bin/aegis-runtime --daemon
fi

# Drop root privileges and run runtime in foreground as PID 1 child under tini.
# With `sh -c`, first arg after `--` becomes $0, so exec must include $0.
exec su -s /bin/sh aegis -c 'exec "$0" "$@"' -- "$@"
