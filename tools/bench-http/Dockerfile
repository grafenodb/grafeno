# The engine and the load tools (crates/grafeno-server/examples/http_load.rs,
# and many_connections.rs, a server and thousands of connections in one process)
# for tools/bench-http/run.sh: one image per C library and allocator, so
# that the same load can be measured on each. From the repository's root:
#
#   docker build -f tools/bench-http/Dockerfile --build-arg TARGET=x86_64-unknown-linux-musl -t grafeno-bench:musl .
#   docker build -f tools/bench-http/Dockerfile --build-arg TARGET=x86_64-unknown-linux-musl --build-arg FEATURES=mimalloc -t grafeno-bench:musl-mimalloc .
#   docker build -f tools/bench-http/Dockerfile --build-arg TARGET=x86_64-unknown-linux-gnu -t grafeno-bench:gnu .
#
# TARGET x86_64-unknown-linux-musl builds as the release does (a static
# binary with musl's allocator); FEATURES=mimalloc swaps the allocator.

FROM rust:1.99.1-slim-trixie@sha256:f6c4a35d21dbb3c89d7a8d056c193e94fc09037f2b58dd858b09c39a47dd02d7 AS build
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-unknown-linux-musl
WORKDIR /src
COPY . .
ARG TARGET=x86_64-unknown-linux-musl
ARG FEATURES=
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=grafeno-bench-target,target=/src/target \
    find crates -name '*.rs' -exec touch {} + \
    && cargo build --release --locked --target "$TARGET" -p grafeno-server \
        ${FEATURES:+--features "$FEATURES"} --bin grafeno --example http_load --example many_connections --example writer_mix \
    && cp "target/$TARGET/release/grafeno" /grafeno \
    && cp "target/$TARGET/release/examples/http_load" /http_load     && cp "target/$TARGET/release/examples/many_connections" /many_connections     && cp "target/$TARGET/release/examples/writer_mix" /writer_mix

FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2
COPY --from=build /grafeno /usr/local/bin/grafeno
COPY --from=build /http_load /usr/local/bin/http_load
COPY --from=build /many_connections /usr/local/bin/many_connections
COPY --from=build /writer_mix /usr/local/bin/writer_mix
ENTRYPOINT ["grafeno"]
