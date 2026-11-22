# Copyright Andy Wick
# SPDX-License-Identifier: Apache-2.0

# The sdb daemon without the window, for a machine that stores friends' backups or backs up
# folders of its own. See "Running the daemon in Docker" in the README.

# Both stages are the same Debian release: the program is linked against the C library it was
# built with, so the one it runs on must not be older.
FROM rust:1-slim-trixie AS build
RUN apt-get update && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --profile dist --locked --no-default-features && cp target/dist/sdb /sdb

FROM debian:trixie-slim
LABEL org.opencontainers.image.source="https://github.com/awick/super-duper-backup" \
      org.opencontainers.image.description="Super Duper Backup daemon: encrypted, deduplicated backups between friends" \
      org.opencontainers.image.licenses="Apache-2.0"
# The certificates are for cloud destinations. Two folders are kept: /config for the node's
# identity and settings, /backups for what friends store on it.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --create-home sdb && mkdir /config /backups && chown sdb:sdb /config /backups
COPY --from=build /sdb /usr/local/bin/sdb
COPY docker/entrypoint.sh /usr/local/bin/sdb-entrypoint
USER sdb
ENV SDB_HOME=/config SDB_IN_CONTAINER=1
VOLUME ["/config", "/backups"]
ENTRYPOINT ["sdb-entrypoint"]
