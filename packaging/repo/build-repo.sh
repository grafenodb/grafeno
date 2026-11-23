#!/bin/sh
# Adds the release's packages to the APT and DNF repositories and signs
# them, so that `apt install grafeno` and `dnf install grafeno` work and
# updates come with the system's:
#
#   packaging/repo/build-repo.sh DIST SITE URL
#
# DIST holds the release's .deb and .rpm files (dist/linux); SITE is the
# repositories' folder, as published (new packages are added to what it
# holds, so older versions stay installable); URL is where SITE is served
# (https://grafenodb.github.io/grafeno). The key that signs is the secret
# key in GnuPG's keyring whose id is $GRAFENO_GPG_KEY.
#
# SITE gets:
#   grafeno.asc                 the public key (the one-liner and users add it)
#   grafeno.repo                the DNF repository file
#   rpm/<arch>/*.rpm, rpm/repodata/  signed packages; repomd.xml.asc
#   deb/pool/main/*.deb, deb/dists/stable/  Release, InRelease, Release.gpg
#
# With GRAFENO_WITHDRAW=<version>, that version's packages are taken out
# of SITE first (a release withdrawn), and DIST may be empty: the
# repositories are indexed and signed again without them.
#
# Needs: gpg, rpm (rpmsign), createrepo_c, apt-ftparchive (apt-utils).
set -eu

DIST="${1:?usage: build-repo.sh DIST SITE URL}"
SITE="${2:?usage: build-repo.sh DIST SITE URL}"
URL="${3:?usage: build-repo.sh DIST SITE URL}"
KEY="${GRAFENO_GPG_KEY:?set GRAFENO_GPG_KEY to the id of the signing key}"
URL="${URL%/}"

mkdir -p "$SITE/rpm/x86_64" "$SITE/rpm/aarch64" "$SITE/deb/pool/main"
if [ -n "${GRAFENO_WITHDRAW:-}" ]; then
    rm -fv "$SITE"/rpm/*/grafeno-"$GRAFENO_WITHDRAW"-*.rpm "$SITE"/deb/pool/main/grafeno_"$GRAFENO_WITHDRAW"-*.deb
fi
gpg --batch --yes --armor --export "$KEY" > "$SITE/grafeno.asc"

# DNF: each package signed, then the repository's metadata, signed too.
cat > "$HOME/.rpmmacros" <<MACROS
%_signature gpg
%_gpg_name $KEY
%__gpg_sign_cmd %{__gpg} gpg --batch --no-verbose --no-armor --pinentry-mode loopback --passphrase-file /dev/null -u "%{_gpg_name}" -sbo %{__signature_filename} --digest-algo sha256 %{__plaintext_filename}
MACROS
for rpm in "$DIST"/grafeno-*.rpm; do
    [ -e "$rpm" ] || continue
    name="$(basename "$rpm")"
    case "$name" in grafeno-linux-*) continue ;; esac
    arch="$(rpm -qp --qf '%{ARCH}' "$rpm" 2>/dev/null)"
    cp "$rpm" "$SITE/rpm/$arch/$name"
    rpmsign --addsign "$SITE/rpm/$arch/$name" >/dev/null
done
createrepo_c --quiet --update "$SITE/rpm"
gpg --batch --yes --armor --detach-sign -u "$KEY" "$SITE/rpm/repodata/repomd.xml"
cat > "$SITE/grafeno.repo" <<REPO
[grafeno]
name=Grafeno
baseurl=$URL/rpm
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=$URL/grafeno.asc
REPO

# APT: a flat pool, the indexes of each architecture, a signed Release.
for deb in "$DIST"/grafeno_*.deb; do
    [ -e "$deb" ] || continue
    cp "$deb" "$SITE/deb/pool/main/"
done
cd "$SITE/deb"
for arch in amd64 arm64; do
    mkdir -p "dists/stable/main/binary-$arch"
    apt-ftparchive --arch "$arch" packages pool > "dists/stable/main/binary-$arch/Packages"
    gzip -9kf "dists/stable/main/binary-$arch/Packages"
done
apt-ftparchive \
    -o APT::FTPArchive::Release::Origin=Grafeno \
    -o APT::FTPArchive::Release::Label=Grafeno \
    -o APT::FTPArchive::Release::Suite=stable \
    -o APT::FTPArchive::Release::Codename=stable \
    -o APT::FTPArchive::Release::Architectures="amd64 arm64" \
    -o APT::FTPArchive::Release::Components=main \
    release dists/stable > dists/stable/Release
gpg --batch --yes --clearsign -u "$KEY" -o dists/stable/InRelease dists/stable/Release
gpg --batch --yes --armor --detach-sign -u "$KEY" -o dists/stable/Release.gpg dists/stable/Release
echo "Repositories ready in $SITE (served at $URL)."
