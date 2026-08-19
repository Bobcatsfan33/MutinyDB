# mutinyd — the M6 quickstart image (docs/M6-SURFACE.md).
#
# THE QUARANTINE NOTICE GOVERNS: every component in this image is release-quarantined
# (components.lock.json), so this image is the composed-development form for the quickstart and
# the gates — built locally, never published, not a supported artifact until M8.

FROM rust:1 AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p mutinyd --locked

FROM debian:bookworm-slim
COPY --from=builder /src/target/release/mutinyd /usr/local/bin/mutinyd
COPY deploy/quickstart.json /etc/mutinyd/quickstart.json
VOLUME /data
EXPOSE 7654
ENTRYPOINT ["mutinyd", "/etc/mutinyd/quickstart.json"]
