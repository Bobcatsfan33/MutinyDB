# MutinyDB v0.1 developer-release image. Supported for evaluation; production approval remains
# gated on the external-KMS custody receipt documented by MD-7.

FROM rust:1 AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p mutinyd --locked

FROM debian:bookworm-slim
ARG VERSION=0.1.0
LABEL org.opencontainers.image.title="MutinyDB" \
      org.opencontainers.image.description="The agent-native truth-maintenance database" \
      org.opencontainers.image.source="https://github.com/Bobcatsfan33/MutinyDB" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.version="${VERSION}"
RUN groupadd --system mutinydb && useradd --system --gid mutinydb --home-dir /data mutinydb \
    && mkdir -p /data /etc/mutinyd && chown -R mutinydb:mutinydb /data
COPY --from=builder /src/target/release/mutinyd /usr/local/bin/mutinyd
COPY deploy/quickstart.json /etc/mutinyd/quickstart.json
COPY LICENSE /usr/share/licenses/mutinydb/LICENSE
VOLUME /data
EXPOSE 7654
USER mutinydb
ENTRYPOINT ["mutinyd", "/etc/mutinyd/quickstart.json"]
