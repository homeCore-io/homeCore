# HomeCore — core CI recipes

# Run all CI checks
check: fmt clippy test

# Check formatting
fmt:
    cargo fmt --all -- --check

# Run clippy lints (deny only compile-affecting and correctness lints for now)
clippy:
    cargo clippy --workspace --all-targets -- \
        -D clippy::correctness \
        -D clippy::suspicious \
        -A clippy::type_complexity \
        -A clippy::too_many_arguments \
        -A clippy::should_implement_trait

# Run tests
test:
    cargo test --workspace

# Debug build
build:
    cargo build --workspace

# Release build
build-release:
    cargo build --workspace --release
