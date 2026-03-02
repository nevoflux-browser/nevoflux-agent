# NevoFlux Agent Development Commands
# See https://github.com/casey/just for installation

# Default recipe - show available commands
default:
    @just --list

# Build the project in debug mode
build:
    cargo build

# Build the project in release mode
release:
    cargo build --release

# Run all tests
test:
    cargo test --all

# Run unit tests only (lib tests in each crate)
test-unit:
    cargo test --all --lib

# Run integration tests only
test-integration:
    cargo test --all --test '*'

# Run tests with output shown
test-verbose:
    cargo test --all -- --nocapture

# Run a specific test by name
test-one NAME:
    cargo test {{NAME}} -- --nocapture

# Run tests for a specific crate
test-crate CRATE:
    cargo test -p {{CRATE}}

# Run clippy linter
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Run clippy and fix issues automatically
lint-fix:
    cargo clippy --all-targets --all-features --fix --allow-dirty

# Format code
fmt:
    cargo fmt --all

# Check if code is formatted
fmt-check:
    cargo fmt --all -- --check

# Run cargo check (fast compilation check)
check:
    cargo check --all

# Generate documentation
doc:
    cargo doc --all --no-deps

# Generate and open documentation
doc-open:
    cargo doc --all --no-deps --open

# Clean build artifacts
clean:
    cargo clean

# Run the daemon
daemon:
    cargo run -- --daemon

# Run in MCP server mode
mcp:
    cargo run -- --mcp

# Check daemon status
status:
    cargo run -- --status

# Stop the daemon
stop:
    cargo run -- --stop

# Run all quality checks (fmt, lint, test)
ci: fmt-check lint test

# Watch for changes and run tests
watch:
    cargo watch -x test

# Watch for changes and run a specific test
watch-test NAME:
    cargo watch -x "test {{NAME}} -- --nocapture"

# Count lines of code
loc:
    @echo "Lines of Rust code:"
    @find . -name "*.rs" -not -path "./target/*" | xargs wc -l | tail -1

# Show test count
test-count:
    @cargo test --all 2>&1 | grep -E "^test result:" | awk '{sum += $4} END {print "Total tests:", sum}'

# Run benchmarks (if any)
bench:
    cargo bench

# Update dependencies
update:
    cargo update

# Check for outdated dependencies
outdated:
    cargo outdated

# Audit dependencies for security vulnerabilities
audit:
    cargo audit

# Generate coverage report (requires cargo-tarpaulin)
coverage:
    cargo tarpaulin --out Html --output-dir target/coverage

# Run the application with arguments
run *ARGS:
    cargo run -- {{ARGS}}

# Download embedding model for bundled deployment
download-model:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "Downloading multilingual-e5-small embedding model..."
    pip install -q huggingface_hub
    python3 -c "
    from huggingface_hub import snapshot_download
    import os
    cache_dir = 'models/fastembed'
    os.makedirs(cache_dir, exist_ok=True)
    snapshot_download('intfloat/multilingual-e5-small', cache_dir=cache_dir, allow_patterns=['onnx/*'])
    print(f'Model downloaded to {cache_dir}')
    "

# Build and install locally
install:
    cargo install --path .

# Uninstall local installation
uninstall:
    cargo uninstall nevoflux-agent
