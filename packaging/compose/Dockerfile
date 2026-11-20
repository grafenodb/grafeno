# The image of the members of the test cluster (docker-compose.yml, drill.sh):
# `grafeno serve` on port 7810, data in /data. It is a tool for testing
# clusters on one machine, not a way to install Grafeno (use the installers,
# docs/install.md). From the repository's root:
#
#   docker build -f packaging/compose/Dockerfile -t grafeno .

FROM rust:1.99.1-slim-trixie@sha256:f6c4a35d21dbb3c89d7a8d056c193e94fc09037f2b58dd858b09c39a47dd02d7 AS build
WORKDIR /src
COPY . .
# The workspace's own sources are touched first: the cached target may hold
# a build of another version whose files look newer than these.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    find crates -name '*.rs' -exec touch {} + \
    && cargo build --release --locked -p grafeno-server \
    && cp target/release/grafeno /usr/local/bin/grafeno

FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2
RUN useradd --system --uid 10001 --no-create-home grafeno \
    && mkdir /data && chown grafeno /data
COPY --from=build /usr/local/bin/grafeno /usr/local/bin/grafeno
USER grafeno
ENV GRAFENO_DB=/data/grafeno.redb \
    GRAFENO_ADDR=0.0.0.0:7810
VOLUME /data
EXPOSE 7810
ENTRYPOINT ["grafeno"]
CMD ["serve"]
