# The container image of the engine, published as ghcr.io/grafenodb/grafeno
# by the release workflow, for linux/amd64 and linux/arm64. It holds the
# release's static binary (no compiling here), runs as an unprivileged
# user, keeps its data in the volume /data and listens on 7810.
#
# It is built from the Linux release files (packaging/linux/Dockerfile
# writes them to dist/linux). From the repository's root:
#
#   docker build -f packaging/linux/Dockerfile --output dist/linux .
#   docker buildx build -f packaging/docker/Dockerfile --platform linux/amd64,linux/arm64 -t grafeno .
#
# Running it: docs/install.md, "Container image".

# The binary of the platform being built, from the release's .tar.gz.
FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2 AS unpack
ARG TARGETARCH
COPY dist/linux/grafeno-x86_64-unknown-linux-musl.tar.gz dist/linux/grafeno-aarch64-unknown-linux-musl.tar.gz /tmp/
RUN case "$TARGETARCH" in \
        amd64) target=x86_64-unknown-linux-musl ;; \
        arm64) target=aarch64-unknown-linux-musl ;; \
        *) echo "no Grafeno build for $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && mkdir /unpack && tar -xzf "/tmp/grafeno-$target.tar.gz" -C /unpack \
    && install -m 0755 /unpack/grafeno /usr/local/bin/grafeno

FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2
ARG VERSION=dev
ARG REVISION=unknown
LABEL org.opencontainers.image.title="Grafeno" \
      org.opencontainers.image.description="Graph database engine for agent memory and knowledge: HTTP API, MCP server and command line" \
      org.opencontainers.image.source="https://github.com/grafenodb/grafeno" \
      org.opencontainers.image.documentation="https://github.com/grafenodb/grafeno/blob/main/docs/install.md" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.revision="$REVISION"
RUN useradd --system --uid 10001 --home-dir /data --no-create-home grafeno \
    && mkdir /data && chown grafeno:grafeno /data && chmod 0750 /data
COPY --from=unpack /usr/local/bin/grafeno /usr/local/bin/grafeno
COPY packaging/docker/entrypoint.sh /usr/local/bin/grafeno-entrypoint
COPY LICENSE-MIT LICENSE-APACHE /usr/share/doc/grafeno/
USER grafeno
WORKDIR /data
ENV GRAFENO_ADDR=0.0.0.0:7810
VOLUME /data
EXPOSE 7810
ENTRYPOINT ["grafeno-entrypoint"]
CMD ["serve"]
