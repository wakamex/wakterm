## Installing from source

If your system is not covered by the pre-built packages, you can build wakterm from source. wakterm runs on Linux, macOS, Windows 10 or later, and modern Unix systems.

- Install [rustup](https://www.rust-lang.org/tools/install) to get the Rust toolchain on your system.
- Rust 1.71 or later is required.
- Build in release mode with `cargo build --release`.
- Run it via `cargo run --release --bin wakterm` or `target/release/wakterm`.

You will need the system development libraries for your platform; the [`get-deps`](https://github.com/wakamex/wakterm/blob/main/get-deps) script installs them on supported Linux and BSD distributions.

Clone the repository with submodules and build:

```console
$ curl https://sh.rustup.rs -sSf | sh -s
$ git clone --depth=1 --branch=main --recursive https://github.com/wakamex/wakterm.git
$ cd wakterm
$ git submodule update --init --recursive
$ ./get-deps
$ cargo build --release
$ cargo run --release --bin wakterm -- start
```

If you see an error about `zlib`, make sure you initialized the git submodules with `git submodule update --init --recursive`.

### Building without Wayland support on Unix systems

By default, support for both X11 and Wayland is included on Unix systems. If your distribution has X11 without Wayland, build wakterm without Wayland support:

```console
$ cargo build --release --no-default-features --features vendored-fonts
```

Building without X11 is not supported.

### Building on Windows

When installing Rust on Windows, select the Microsoft Visual C++ (MSVC) toolchain target (`x86_64-pc-windows-msvc`).

Instead of running `get-deps`, install [Strawberry Perl](https://strawberryperl.com) and place its `perl.exe` ahead of any other Perl installation in `PATH` so Cargo can build OpenSSL:

```console
$ set PATH=c:\Strawberry\perl\bin;%PATH%
```
