#!/bin/sh
# Copyright Andy Wick
# SPDX-License-Identifier: Apache-2.0

# Sets the node up the first time the container starts, then runs the daemon. The node's
# identity and settings live in /config, what friends store on it in /backups.
#   SDB_NAME  what friends see this node as (default: the container's host name)
#   SDB_PORT  the UDP port to listen on, when one is published with -p (default: picked at random)
set -e
if [ ! -f "$SDB_HOME/identity.key" ]; then
    sdb init --name "${SDB_NAME:-$(hostname)}" --store /backups ${SDB_PORT:+--port "$SDB_PORT"}
fi
exec sdb daemon
