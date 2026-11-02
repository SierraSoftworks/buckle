# NOTE: This Dockerfile expects a prebuilt `buckle` binary for the target
# platform in the build context (CI builds it per platform first). Keeping the
# compile outside Docker makes cross-platform images fast; the image itself is
# just a base with CA certificates and the static binary.
FROM ubuntu:24.04
LABEL org.opencontainers.image.source=https://github.com/SierraSoftworks/buckle
LABEL org.opencontainers.image.description="Lightweight fleet configuration with phased, health-gated rollouts"
LABEL org.opencontainers.image.licenses=MIT
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*
ADD ./buckle /usr/local/bin/buckle
WORKDIR /data
VOLUME /data
ENV BUCKLE_SERVER_CONFIG=/config/server.yaml
ENTRYPOINT ["/usr/local/bin/buckle"]
CMD ["server"]
