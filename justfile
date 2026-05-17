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
    snapshot_download('intfloat/multilingual-e5-small', cache_dir=cache_dir, allow_patterns=['onnx/model_O4.onnx', 'onnx/tokenizer.json', 'onnx/config.json', 'onnx/special_tokens_map.json', 'onnx/tokenizer_config.json', 'onnx/sentencepiece.bpe.model'])
    import glob, shutil
    for f in glob.glob(os.path.join(cache_dir, '**', 'model_O4.onnx'), recursive=True):
        target = os.path.join(os.path.dirname(f), 'model.onnx')
        os.rename(f, target)
        print(f'Renamed {f} -> {target}')
    # Resolve symlinks and remove blobs to avoid duplicate data
    model_dir = os.path.join(cache_dir, 'models--intfloat--multilingual-e5-small')
    for root, dirs, files in os.walk(os.path.join(model_dir, 'snapshots')):
        for fname in files:
            fpath = os.path.join(root, fname)
            if os.path.islink(fpath):
                real = os.path.realpath(fpath)
                os.remove(fpath)
                shutil.copy2(real, fpath)
    blobs = os.path.join(model_dir, 'blobs')
    if os.path.isdir(blobs):
        shutil.rmtree(blobs)
    print(f'Model downloaded to {cache_dir}')
    "

# Build and install locally
install:
    cargo install --path .

# Uninstall local installation
uninstall:
    cargo uninstall nevoflux-agent

# --- Eval recipes (Phase 2) ---

# Daemon-only eval — fastest path, no real browser, Exploratory grade.
eval-daemon BENCHMARK="nevoflux-suite":
    cargo build --release --bin nevoflux-agent
    cargo run --release -p nevoflux-eval -- run \
        --benchmark {{BENCHMARK}} \
        --browser-mode daemon-only \
        --out-dir eval/reports

# External dev-instance eval — connects to a locally-running nevoflux dev
# instance whose daemon was launched with NEVOFLUX_DEV_INSTANCE_MODE=1.
# See eval/README-EXTERNAL-MODE.md for setup.
eval-dev BENCHMARK="online-mind2web" LIMIT="3":
    cargo run --release -p nevoflux-eval -- run \
        --benchmark {{BENCHMARK}} \
        --browser-mode external \
        --limit {{LIMIT}} \
        --out-dir eval/reports

# Release-mode eval (Phase 4 stub for now — errors out cleanly).
eval-release VERSION BENCHMARK="online-mind2web":
    cargo run --release -p nevoflux-eval -- run \
        --benchmark {{BENCHMARK}} \
        --browser-mode release \
        --browser-version {{VERSION}} \
        --out-dir eval/reports

# List registered benchmarks + judges.
eval-list:
    cargo run --release -p nevoflux-eval -- list

# Mock-LLM eval — deterministic + free, suitable for CI. Builds the daemon
# binary with the eval-mock-llm feature flag enabled. Phase 3a.
eval-mock BENCHMARK="nevoflux-suite":
    cargo build --release --bin nevoflux-agent --features eval-mock-llm
    NEVOFLUX_EVAL_LLM_MODE=mock cargo run --release -p nevoflux-eval -- run \
        --benchmark {{BENCHMARK}} \
        --browser-mode daemon-only \
        --filter mode-authz \
        --timeout 30 \
        --out-dir eval/reports

# Fetch the upstream benchmark datasets (BrowseComp, BrowseComp-ZH,
# Online-Mind2Web). NOT run by default — eval CI uses fixtures.
# See eval/README-DATASETS.md for the manual playbook.
eval-fetch-data:
    @echo "See eval/README-DATASETS.md for the manual fetch playbook."
    @echo "BrowseComp: XOR-encrypted CSV at OpenAI blob (~\$20-30 LLM cost to run)"
    @echo "BrowseComp-ZH: HuggingFace Phantom-AI/BrowseComp-ZH parquet"
    @echo "Online-Mind2Web: 450MB GitHub clone, derive tasks from per-id result.json files"
    @echo ""
    @echo "Phase 3c ships 5-task fixtures for all three; this recipe is a"
    @echo "placeholder for the full integration in Phase 3d/4."
    @false

# Fetch + cache the BrowseComp encrypted CSV from OpenAI blob storage.
# After running, set NEVOFLUX_BC_DATA_PATH=eval/benchmarks/browsecomp.csv
# and the browsecomp adapter will use the real 1266-task upstream data.
eval-fetch-bc:
    @mkdir -p eval/benchmarks
    curl -fsSL \
        "https://openaipublic.blob.core.windows.net/simple-evals/browse_comp_test_set.csv" \
        -o eval/benchmarks/browsecomp.csv
    @echo "Fetched $(wc -l < eval/benchmarks/browsecomp.csv) lines into eval/benchmarks/browsecomp.csv"
    @echo "Now run with: NEVOFLUX_BC_DATA_PATH=eval/benchmarks/browsecomp.csv just eval-mock browsecomp"
