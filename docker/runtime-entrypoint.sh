#!/bin/sh
set -eu

# Determine container runtime socket path.
# Priority: CONTAINER_HOST env > DOCKER_HOST env > well-known paths.
SOCK_PATH=""
for candidate in \
    "${CONTAINER_HOST#unix://}" \
    "${DOCKER_HOST#unix://}" \
    "/run/podman/podman.sock" \
    "/var/run/docker.sock"; do
  if [ -S "$candidate" ] 2>/dev/null; then
    SOCK_PATH="$candidate"
    break
  fi
done

if [ -n "$SOCK_PATH" ]; then
  if stat -c '%g' /dev/null >/dev/null 2>&1; then
    SOCK_GID="$(stat -c '%g' "$SOCK_PATH" 2>/dev/null || true)"
  elif stat -f '%g' /dev/null >/dev/null 2>&1; then
    SOCK_GID="$(stat -f '%g' "$SOCK_PATH" 2>/dev/null || true)"
  else
    SOCK_GID=""
  fi
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
