FROM rust:1.97.1 AS builder

WORKDIR /usr/src/livepeer-funds-transfer
COPY . .
RUN cargo install --path .

FROM debian:latest
# The HTTP client (reqwest 0.13 + rustls) verifies TLS against the OS trust store, which a bare
# debian image does not ship. Without it the binary panics at startup with
# "No CA certificates were loaded from the system".
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/cargo/bin/funds_transfer /usr/local/bin/funds_transfer
WORKDIR /root/
CMD ["funds_transfer"]