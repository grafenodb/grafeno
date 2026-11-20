# Builds the Linux release files of the engine, for x86_64 and aarch64: a
# static (musl) binary as a .tar.gz, a .deb and an .rpm, with SHA-256
# checksums. No interface, no graphical dependency. The packages also come
# under names without the version (grafeno-linux-amd64.deb,
# grafeno-linux-x86_64.rpm, grafeno-linux-arm64.deb,
# grafeno-linux-aarch64.rpm), which install.sh asks the latest release for.
# The .rpm carries the SELinux policy module, the .deb the AppArmor profile.
#
#   docker build -f packaging/linux/Dockerfile --output dist/linux .
#
# One architecture only: --build-arg TARGETS=x86_64-unknown-linux-musl

# The SELinux module, compiled with the policy tools of RHEL's family.
FROM almalinux:9@sha256:02df05719ef287ebd0df0a1eedb9172a00bd70703aed7b25660e426b8e0d6047 AS selinux
RUN dnf install -y -q selinux-policy-devel make && dnf clean all
COPY packaging/linux/selinux /selinux
WORKDIR /selinux
RUN make -f /usr/share/selinux/devel/Makefile grafeno.pp

# Images, tools and the zig wheel are pinned: a new version comes with a
# commit of this repository, not with whoever publishes them.
FROM rust:1.99.1-slim-trixie@sha256:f6c4a35d21dbb3c89d7a8d056c193e94fc09037f2b58dd858b09c39a47dd02d7 AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools rpm dpkg-dev file python3-pip \
    && printf '%s\n' "ziglang==0.13.0 \\" \
        "    --hash=sha256:3ce0c9f16547e5d61b32e0d226926e9a2552ef4b91fccf7ab5ea1a623a77824b \\" \
        "    --hash=sha256:5714a4d46f9246ba6180a0930e23df2bc8bc26e62b5fe0dc798b4afca1903308" \
        > /tmp/zig.txt \
    && pip install --break-system-packages --no-cache-dir --require-hashes -r /tmp/zig.txt \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install --locked --version 3.8.0 cargo-deb \
    && cargo install --locked --version 0.21.1 cargo-generate-rpm \
    && cargo install --locked --version 0.24.1 cargo-zigbuild
WORKDIR /src
COPY . .
COPY --from=selinux /selinux/grafeno.pp /selinux/grafeno.pp
ARG TARGETS="x86_64-unknown-linux-musl aarch64-unknown-linux-musl"
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    TARGETS="$TARGETS" sh packaging/linux/build-packages.sh

FROM scratch
COPY --from=build /out /
