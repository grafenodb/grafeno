#!/bin/sh
# Builds the binary, the .tar.gz, the .deb and the .rpm of each target in
# $TARGETS into /out (run by packaging/linux/Dockerfile). x86_64 links
# with musl-gcc; aarch64 is cross-compiled with Zig (cargo zigbuild).
set -eu
mkdir -p /out target/selinux
cp /selinux/grafeno.pp target/selinux/grafeno.pp
# Symbols stripped by Cargo, for every architecture alike.
export CARGO_PROFILE_RELEASE_STRIP=symbols
for TARGET in $TARGETS; do
    case "$TARGET" in
        x86_64-*) DEB=amd64; RPM=x86_64; BUILD="cargo build" ;;
        aarch64-*) DEB=arm64; RPM=aarch64; BUILD="cargo zigbuild" ;;
        *) echo "unknown target $TARGET" >&2; exit 1 ;;
    esac
    # Cargo's cache mount keeps crates across versions: touch ours so they
    # are always rebuilt from these sources.
    find crates -name '*.rs' -exec touch {} +
    $BUILD --release --locked --target "$TARGET" -p grafeno-server
    file "target/$TARGET/release/grafeno"
    cargo deb --no-build --no-strip --target "$TARGET" -p grafeno-server --output /out/
    cargo generate-rpm --target "$TARGET" -p crates/grafeno-server --output /out/
    cp /out/grafeno_*_"$DEB".deb "/out/grafeno-linux-$DEB.deb"
    cp /out/grafeno-*."$RPM".rpm "/out/grafeno-linux-$RPM.rpm"
    staging="$(mktemp -d)"
    cp "target/$TARGET/release/grafeno" packaging/linux/grafeno.service \
        packaging/linux/grafeno@.service packaging/linux/setup.sh packaging/linux/stop-all.sh \
        README.md LICENSE-MIT LICENSE-APACHE "$staging/"
    tar -czf "/out/grafeno-$TARGET.tar.gz" -C "$staging" .
    rm -rf "$staging"
done
cd /out
for f in *; do
    case "$f" in *.sha256) continue ;; esac
    sha256sum "$f" | sed "s| .*|  $f|" > "$f.sha256"
done
ls -l /out
