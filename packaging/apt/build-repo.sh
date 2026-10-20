#!/bin/sh
# Builds a signed APT repository out of .deb files.
#
# usage: build-repo.sh DEB_DIR OUT_DIR
#
# Signs with the first secret key in gpg's keyring and exports its public
# half as wbanish.asc next to the repository.
set -eu

debs=$1
out=$2
here=$(cd "$(dirname "$0")" && pwd)
archs="amd64 arm64"

key=$(gpg --batch --with-colons --list-secret-keys | awk -F: '$1 == "fpr" { print $10; exit }')
if [ -z "$key" ]; then
	echo "build-repo.sh: no secret key to sign with" >&2
	exit 1
fi

mkdir -p "$out/pool/main"
cp "$debs"/*.deb "$out/pool/main/"
cd "$out"

for arch in $archs; do
	dir=dists/stable/main/binary-$arch
	mkdir -p "$dir"
	apt-ftparchive --arch "$arch" packages pool/main > "$dir/Packages"
	gzip -9 -k -f "$dir/Packages"
done

# Written outside dists/ first, or Release would list itself.
apt-ftparchive \
	-o APT::FTPArchive::Release::Origin=wbanish \
	-o APT::FTPArchive::Release::Label=wbanish \
	-o APT::FTPArchive::Release::Suite=stable \
	-o APT::FTPArchive::Release::Codename=stable \
	-o APT::FTPArchive::Release::Architectures="$archs" \
	-o APT::FTPArchive::Release::Components=main \
	release dists/stable > Release.new
mv Release.new dists/stable/Release

gpg --batch --yes --local-user "$key" --armor --detach-sign -o dists/stable/Release.gpg dists/stable/Release
gpg --batch --yes --local-user "$key" --clearsign -o dists/stable/InRelease dists/stable/Release
gpg --armor --export "$key" > wbanish.asc

cp "$here/index.html" index.html
