# Packaging

## APT repository

Pushing a `v*` tag runs [release.yml](../.github/workflows/release.yml).
It builds static amd64 and arm64 .debs with cargo-deb, attaches them to a
GitHub release, and pushes a signed APT repository, made by
[apt/build-repo.sh](apt/build-repo.sh), to the `gh-pages` branch.
The repository only holds the latest release.

One-time setup:

1. Create a signing key without a passphrase, and copy it (`clip.exe`
   copies to the Windows clipboard under WSL):

   ```sh
   gpg --batch --passphrase '' --quick-gen-key \
       "wbanish APT repository <komiljon.sobitov.uzb@gmail.com>" rsa4096 cert,sign never
   gpg --armor --export-secret-keys "wbanish APT repository" | clip.exe
   ```

2. Add it as the `APT_SIGNING_KEY` secret under Settings → Secrets and
   variables → Actions.
3. After the first release has run, set Settings → Pages → Source to
   "Deploy from a branch", `gh-pages`, `/ (root)`.

Keep a backup of the key: users trust it, and replacing it means every
user has to download the new one.

To release, bump the version in Cargo.toml, commit, then:

```sh
git tag v0.1.0
git push origin v0.1.0
```

## Debian

wbanish is packaged by the
[Debian Rust Team](https://rust-team.pages.debian.net/book/) from the crate
on crates.io, using
[debcargo-conf](https://salsa.debian.org/rust-team/debcargo-conf).
`debcargo-conf/src/wbanish/debian/` holds the files that go into that
repository.

Every dependency must already be in Debian unstable, and Cargo.toml's
lower bounds are the versions there.
Before raising a dependency, check it with
`curl 'https://qa.debian.org/madison.php?package=rust-<crate>&text=on'`.

To get a release into Debian:

1. Tag the release and publish it with `cargo publish`.
2. File the ITP by mailing [ITP.txt](ITP.txt) to submit@bugs.debian.org.
   Put the bug number you get back in place of `NNNNNN` in
   `debian/changelog` and `debian/RFS`.
3. Create a [salsa](https://salsa.debian.org) account, fork debcargo-conf
   and copy `debcargo-conf/src/wbanish` into `src/wbanish` of the fork.
4. In Debian unstable, run `./update.sh wbanish` and `./build.sh wbanish`,
   and fix whatever they report.
5. Open a merge request with a commit titled `wbanish: new package`, then
   ask for a sponsor in
   [#debian-rust](https://matrix.to/#/#debian-rust:matrix.debian.social).
6. After the upload, the package waits in the
   [NEW queue](https://ftp-master.debian.org/new.html) until the ftpmasters
   accept it, and then migrates from unstable to testing.
   Ubuntu imports it from Debian.
