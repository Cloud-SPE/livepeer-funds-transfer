# Builder and runtime are pinned to the same Debian release (13, trixie). The binary links
# against the builder's glibc, so the runtime must not be older.
FROM rust:1.97.1-trixie AS builder

WORKDIR /usr/src/livepeer-funds-transfer
# Only Cargo.toml, Cargo.lock and src/ reach the build (see .dockerignore).
COPY . .
# --locked builds the exact dependency versions in Cargo.lock; `cargo install` ignores the
# lockfile without it.
RUN cargo install --locked --path .

FROM debian:trixie-slim
# The HTTP client (reqwest 0.13 + rustls) verifies TLS against the OS trust store, which a bare
# debian image does not ship. Without it the binary panics at startup with
# "No CA certificates were loaded from the system".
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# Run as an unprivileged user with a fixed UID/GID (10001), so key files on the host or in a
# volume can be granted to it. /keys is the mount point for the keystore and passphrase files;
# a new named volume mounted there starts out owned by this user.
RUN groupadd --system --gid 10001 app \
    && useradd --system --uid 10001 --gid 10001 --home-dir /home/app --create-home \
        --shell /usr/sbin/nologin app \
    && install -d -o 10001 -g 10001 -m 0700 /keys
COPY --from=builder /usr/local/cargo/bin/funds_transfer /usr/local/bin/funds_transfer
USER 10001:10001
WORKDIR /home/app
CMD ["funds_transfer"]
