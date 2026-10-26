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
sudo install -m 0755 target/release/remoteplayer-mpvwatcher /usr/local/bin/
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

Building on a PC and copying the binary over is much faster. Two common tools:

- [`cross`](https://github.com/cross-rs/cross) (uses Docker or Podman):

  ```sh
  cargo install cross
  cross build --release --target aarch64-unknown-linux-gnu        # 64-bit Raspberry Pi OS
  cross build --release --target armv7-unknown-linux-gnueabihf    # 32-bit Raspberry Pi OS
  ```

- [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) (uses Zig as the linker, no containers):

  ```sh
  cargo install cargo-zigbuild    # also needs `zig` installed
  rustup target add aarch64-unknown-linux-gnu
  cargo zigbuild --release --target aarch64-unknown-linux-gnu
  ```

The result is in `target/<target>/release/`; copy it to the Pi with `scp`.
