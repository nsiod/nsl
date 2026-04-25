set dotenv-load := false

# Default: run all checks
default: check test

# ─── Build ────────────────────────────────────────────────────────────

# Build debug binary
build:
    cargo build

# Build release binary
release:
    cargo build --release

# ─── Quality Gates ────────────────────────────────────────────────────

# Run all quality checks (fmt + clippy + deny)
check: fmt-check lint deny

# Check formatting
fmt-check:
    cargo fmt --all -- --check

# Apply formatting
fmt:
    cargo fmt --all

# Run clippy with strict warnings
lint:
    cargo clippy --all-targets -- -D warnings

# Run cargo-deny (advisories + licenses + bans)
deny:
    cargo deny check

# ─── Unit Tests ───────────────────────────────────────────────────────

# Run Rust unit tests
unit:
    cargo test --all-targets

# ─── E2E Tests ────────────────────────────────────────────────────────

# Run all e2e tests
e2e: _e2e-deps
    cd tests && bun test --timeout 30000 src/

# Run proxy e2e tests (lifecycle, forwarding, websocket, errors)
e2e-proxy: _e2e-deps
    cd tests && bun test --timeout 30000 src/proxy/

# Run routing e2e tests (route, path-prefix, change-origin)
e2e-routing: _e2e-deps
    cd tests && bun test --timeout 30000 src/routing/

# Run CLI e2e tests (commands, flags)
e2e-cli: _e2e-deps
    cd tests && bun test --timeout 30000 src/cli/

# Install e2e dependencies (internal)
_e2e-deps:
    cd tests && bun install --frozen-lockfile 2>/dev/null || cd tests && bun install

# ─── Combined ─────────────────────────────────────────────────────────

# Run all tests (unit + e2e)
test: unit e2e

# Full CI pipeline (check + unit + e2e + release build)
ci: check unit e2e release

# ─── npm Packaging ────────────────────────────────────────────────────

# Sync npm package versions from Cargo.toml
npm-sync:
    node npm/sync-version.mjs

# Stage the local musl binary into the linux-x64 subpackage (for dry-run)
npm-stage-local:
    cargo build --release --target x86_64-unknown-linux-musl
    mkdir -p npm/linux-x64/bin
    cp target/x86_64-unknown-linux-musl/release/nsl npm/linux-x64/bin/nsl
    chmod +x npm/linux-x64/bin/nsl

# Dry-run: pack the wrapper + linux-x64 subpackage to .tgz (inspect only)
npm-pack-dry: npm-sync npm-stage-local
    cd npm/linux-x64 && npm pack --dry-run
    cd npm/nsl && npm pack --dry-run

# ─── Docker (nsld server image) ───────────────────────────────────────

# Build a host-arch nsld image from source via Dockerfile.local. No
# QEMU, no multi-arch staging — purely for local sanity-check.
docker-build-local:
    docker build -f crates/nsld/Dockerfile.local -t nsld:local .

# Smoke-run the local image — prints the resolved config and exits.
docker-run-local: docker-build-local
    mkdir -p data
    docker run --rm -v "$(pwd)/data:/data" nsld:local config || true
