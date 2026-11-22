#!/bin/sh
# Copyright Andy Wick
# SPDX-License-Identifier: Apache-2.0

# Sets the node up the first time the container starts, then runs the daemon. The node's
# identity and settings live in /config, what friends store on it in /backups.
#   CAHOOTS_NAME  what friends see this node as (default: the container's host name)
#   CAHOOTS_PORT  the UDP port to listen on, when one is published with -p (default: picked at random)
# SDB_NAME and SDB_PORT, as they were called before the rename, still work.
set -e
NAME=${CAHOOTS_NAME:-${SDB_NAME:-$(hostname)}}
PORT=${CAHOOTS_PORT:-${SDB_PORT:-}}
if [ ! -f "$CAHOOTS_HOME/identity.key" ]; then
    cahoots init --name "$NAME" --store /backups ${PORT:+--port "$PORT"}
fi
exec cahoots daemon
