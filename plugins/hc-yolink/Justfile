set shell := ["bash", "-uc"]

# Plugin name auto-derived from the directory containing this Justfile,
# so the same template ships unchanged to every hc-* plugin.
BIN_NAME := file_name(justfile_directory())

# hc-scripts location (override with `HC_SCRIPTS=... just package` if your
# checkout doesn't follow the meta-layout).
HC_SCRIPTS := env_var_or_default("HC_SCRIPTS", "../../hc-scripts")

# List recipes
default:
    @just --list

# fmt + clippy + test (mirrors hc-scripts CI workflow)
check: fmt clippy test

fmt:
    cargo fmt --all -- --check

fmt-fix:
    cargo fmt --all

clippy:
    cargo clippy --all-targets --all-features -- \
        -D warnings \
        -A clippy::too_many_arguments \
        -A clippy::type_complexity \
        -A clippy::result_large_err

test:
    cargo test --all-features

build:
    cargo build

build-release:
    cargo build --release

run:
    cargo run -- --config config/config.dev.toml

clean:
    cargo clean

# Build + package as a homecore/plugins/<name>/ fragment tarball under dist/
package:
    {{ HC_SCRIPTS }}/build-archive.sh --kind plugin --name {{ BIN_NAME }} --build
