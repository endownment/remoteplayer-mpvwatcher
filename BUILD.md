# Building remoteplayer-mpvwatcher

The project is plain Cargo with a handful of dependencies and no C libraries
to link. All you need is a Rust toolchain and a C linker.

Minimum Rust version: **1.85** (`rust-version` in `Cargo.toml`, edition 2024).
`Cargo.lock` is committed, so every build uses exactly the tested dependency
versions.

## On the Raspberry Pi 3 itself

The watcher is small (no HTTP server, no D-Bus), so it builds on a Pi 3 much
faster than remoteplayer-restservice. Memory is still the limit when several
`rustc` processes run at once; the steps below keep it reliable.

### 1. Toolchain

Either one works; check afterwards that `rustc --version` reports 1.85 or newer.

**Debian packages** — simplest, updated with the system:

```sh
sudo apt install rustc cargo build-essential git
```

**rustup** — the upstream installer, needed if the packaged Rust is older than 1.85:

```sh
sudo apt install build-essential git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
```

Both 64-bit (`uname -m` → `aarch64`) and 32-bit (`armv7l`) Raspberry Pi OS work.

### 2. Enough memory

Check `free -h`. With less than ~1.5 GB of RAM + swap combined, add a
temporary swap file for the build:

```sh
sudo fallocate -l 2G /var/swap.build
sudo chmod 600 /var/swap.build
sudo mkswap /var/swap.build
sudo swapon /var/swap.build
# after the build:
sudo swapoff /var/swap.build && sudo rm /var/swap.build
```

Stopping mpv during the build also frees memory:
`sudo systemctl stop remoteplayer-mpv`.

### 3. Build

```sh
git clone https://github.com/endownment/remoteplayer-mpvwatcher.git
cd remoteplayer-mpvwatcher
cargo build --release --locked -j 2
```

- `-j 2` limits parallel compiler processes; with more swap you can try `-j 3`.
- `--locked` refuses to build if `Cargo.lock` would have to change.
- The first build downloads and compiles all dependencies; on a Pi 3 that
  takes several minutes. Later builds only recompile what changed.

The binary is `target/release/remoteplayer-mpvwatcher`. Installation is
described in [README.md](README.md#installation-on-the-raspberry-pi).

### 4. Updating

```sh
git pull
cargo build --release --locked -j 2
sudo install -m 0755 target/release/remoteplayer-mpvwatcher /usr/bin/
sudo systemctl restart remoteplayer-mpvwatcher
```

## On a development machine

Any Linux with Rust ≥ 1.85 (install with rustup as above) and, to run it
against a real player, mpv.

```sh
cargo build                                  # debug build
cargo run -- --config config/dev.toml        # run with the desktop configuration
cargo test                                   # unit tests, with an in-process fake mpv
cargo clippy --all-targets                   # lints; the code is kept warning-free
cargo fmt                                    # formatting (rustfmt.toml: 120 columns)
```

## Cross-compiling for the Pi (optional)

Building on a PC and copying the binary over is much faster. On Debian or
Ubuntu the plain GNU cross linkers are enough — the project links no C
libraries — and `.cargo/config.toml` already names them:

```sh
sudo apt install gcc-aarch64-linux-gnu gcc-arm-linux-gnueabihf
rustup target add aarch64-unknown-linux-gnu armv7-unknown-linux-gnueabihf
cargo build --release --locked --target aarch64-unknown-linux-gnu        # 64-bit Raspberry Pi OS
cargo build --release --locked --target armv7-unknown-linux-gnueabihf    # 32-bit Raspberry Pi OS
```

The result is in `target/<target>/release/`; copy it to the Pi with `scp`.
`armv7` binaries run on every Pi from the Pi 2 on (the 32-bit OS on a Pi 3
or 4 included), not on a Pi 1 or Pi Zero (ARMv6).

The binary needs the glibc of the machine it was built on, at least. Built on
a newer distribution than the Pi runs, it fails to start with
"GLIBC_2.xx not found" — the CI builds inside Debian 12 for that reason.

## Debian packages

`[package.metadata.deb]` in `Cargo.toml` describes the package for
[cargo-deb](https://github.com/kornelski/cargo-deb): files, dependencies, and
the maintainer scripts in `packaging/debian/` (create the `remoteplayer` user
and directories, reload systemd, restart the running service on upgrade,
disable it on removal).

```sh
cargo install cargo-deb --locked
cargo deb --locked                                                       # for this machine
cargo deb --locked --no-strip --target aarch64-unknown-linux-gnu         # arm64 package on a PC
cargo deb --locked --no-strip --target armv7-unknown-linux-gnueabihf     # armhf package on a PC
```

The packages land in `target/debian/`. On the Pi itself `cargo deb` builds a
package for the Pi's own architecture (with the memory precautions above).

### GitHub Actions

`.github/workflows/packages.yml` does all of this on every push:

1. **check** — `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`,
   with Rust 1.85 (the minimum the project supports);
2. **package** — for `amd64`, `arm64` and `armhf`, inside a `debian:bookworm`
   container: builds the `.deb`, lists its contents and fails if the binary
   needs a newer glibc than 2.36 (the `libc6 (>= 2.36)` dependency);
3. **release** — only for a tag `vX.Y.Z` that matches `version` in
   `Cargo.toml`: publishes the three packages as a GitHub release and
   notifies [remoteplayer-apt](https://github.com/endownment/remoteplayer-apt),
   which adds them to the APT repository (secret `APT_REPOSITORY_TOKEN`;
   without it within a day).

Packages of an ordinary push are attached to the workflow run (Actions →
the run → Artifacts). To publish a release:

```sh
# bump `version` in Cargo.toml, commit, then
git tag v0.1.1 && git push origin v0.1.1
```
