# HomeCore — core CI recipes

# Run all CI checks
check: fmt clippy test

# Check formatting
fmt:
    cargo fmt --all -- --check

# Run clippy exactly as CI does (hc-scripts rust-ci.yml), so a green `just
# check` means a green CI lint step.
clippy:
    cargo clippy --workspace --all-targets --all-features -- \
        -D warnings \
        -A clippy::type_complexity

# Run tests
test:
    cargo test --workspace

# Debug build
build:
    cargo build --workspace

# Release build
build-release:
    cargo build --workspace --release

# Remove build artifacts
clean:
    cargo clean
