# Contributing

## Build

The default build is pure Rust and has only the wlroots backend. Rust is pinned by `rust-toolchain.toml`.

```sh
cargo build --release --features gnome,xvfb
```

The `gnome` feature adds the GNOME backend. The `xvfb` feature adds the stock engines. Build dependencies per distribution are listed in the install documentation.

## Tests

```sh
cargo test --features gnome,xvfb
cargo test
bash packaging/tests/test_ctl.sh
python3 -m unittest discover -s tools/x11-a11y-proxy/tests -p test_unit.py
```

- The first command runs 351 unit tests. They need no display and no RealVNC.
- `cargo test` runs 228 tests with default features.
- `test_ctl.sh` runs 99 checks of `xwlbridge-ctl`, `install.sh`, `uninstall.sh` and migrate. It uses fakes only.
- The proxy unit tests need PyGObject with GTK 4 and Atspi typelibs.
- Container tests in `tools/x11-a11y-proxy/tests/*.sh` need a GNOME test-bed image (see `tools/x11-a11y-proxy/tests/bed/README.md`).

CI runs on GitHub Actions: `cargo fmt --check`, clippy, tests, a release build, `test_ctl.sh` and the proxy unit tests.

## Formatting and lints

```sh
cargo fmt
cargo clippy --all-targets --features gnome,xvfb -- -D warnings
```

## Commits

- One logical change per commit.
- Say what was tested, and where.

## Documentation

- Every flag a document mentions must exist in `xwlbridge -help`.
- Claims say what was tested and where. Say "not tested" when it was not.

## Licence

xwlbridge is MIT licensed. Contributions are accepted under the same licence.

## Upstream

xwlbridge is based on xwlrvnc by Patrick Gaskin (https://github.com/pgaskin/xwlrvnc). Fixes that apply to upstream's code may be offered there too.
