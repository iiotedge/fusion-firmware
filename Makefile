# ==============================================================================
# Fusion Firmware Makefile (Production / CI-Ready)
# Reference target: Radxa Zero 3E (aarch64-unknown-linux-gnu); i.MX 8M Plus supported
# ==============================================================================

APP_NAME          := fusion-firmware
TARGET_ARCH       := aarch64-unknown-linux-gnu
DOCKER_IMG        := iiotedge-builder

# --- Device (override per invocation: make deploy DEVICE_IP=10.0.0.7) ---------
DEVICE_IP         ?= 192.168.1.17
DEVICE_USER       ?= radxa
DEVICE_DIR        ?= /home/$(DEVICE_USER)/iiotedge
DEVICE            := $(DEVICE_USER)@$(DEVICE_IP)

# --- Versioning ----------------------------------------------------------------
VERSION           := $(shell grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
GIT_HASH          := $(shell git rev-parse --short HEAD 2>/dev/null || echo nogit)
DIST_NAME         := $(APP_NAME)-$(VERSION)-$(GIT_HASH)-$(TARGET_ARCH)

# Directory and Volume Configuration for High-Speed Caching
DOCKER_TARGET_DIR := target-docker
CARGO_CACHE_VOL   := iiotedge-cargo-cache
CARGO_GIT_VOL     := iiotedge-cargo-git

# Reusable Docker run command with named volume caching and target isolation.
# ../iiotedge-lib is mounted at /iiotedge-lib so the manifest's relative path
# dependencies (../iiotedge-lib/crates/*) resolve inside the container.
IIOTEDGE_LIB_DIR  ?= $(abspath $(PWD)/../iiotedge-lib)
DOCKER_RUN := docker run --rm --init \
	-v "$(PWD)":/workspace \
	-v "$(IIOTEDGE_LIB_DIR)":/iiotedge-lib \
	-v $(CARGO_CACHE_VOL):/root/.cargo/registry \
	-v $(CARGO_GIT_VOL):/root/.cargo/git \
	-e CARGO_TARGET_DIR=/workspace/$(DOCKER_TARGET_DIR) \
	-e CARGO_TERM_COLOR=always \
	$(DOCKER_IMG)

RELEASE_BIN := $(DOCKER_TARGET_DIR)/$(TARGET_ARCH)/release/$(APP_NAME)

.PHONY: all build release run clean fmt fmt-check lint test ci \
        docker-image docker-build docker-release docker-check docker-lint docker-shell docker-clean \
        dist deb deploy deploy-config install-service device-status device-logs device-restart device-shell \
        version help

.DEFAULT_GOAL := help
all: help

# ==============================================================================
# Local Development (Runs natively on your current host machine)
# ==============================================================================

build: ## Build the firmware for your local host machine (Debug mode)
	@echo "🛠️  Building locally (Debug)..."
	cargo build

release: ## Build the firmware for your local host machine (Release mode)
	@echo "🚀 Building locally (Release)..."
	cargo build --release

run: build ## Build and run locally (mock camera on macOS/Windows)
	@echo "▶️  Running $(APP_NAME) locally..."
	./target/debug/$(APP_NAME)

clean: ## Clean local and Docker build artifacts
	@echo "🧹 Cleaning build artifacts..."
	cargo clean
	rm -rf $(DOCKER_TARGET_DIR) dist

# ==============================================================================
# Quality Gates (same checks CI runs — run `make ci` before pushing)
# ==============================================================================

fmt: ## Format all Rust code in place
	cargo fmt

fmt-check: ## Fail if code is not rustfmt-clean
	# No --all: sweeps in the sibling iiotedge-lib/iiotedge-sdk path
	# dependency via cargo metadata's resolve graph even without a
	# workspace (a cargo-fmt quirk) - not this repo's to fix if that
	# SDK's own source has an issue.
	cargo fmt -- --check

lint: ## Clippy with warnings as errors (host target)
	cargo clippy --all-targets -- -D warnings

test: ## Run the test suite (host target)
	cargo test

ci: fmt-check lint test ## Full local CI gate: fmt + clippy + tests
	@echo "✅ CI gate passed."

# ==============================================================================
# Docker Cross-Compilation (aarch64: Radxa Zero 3E / i.MX 8M Plus)
# ==============================================================================

docker-image: ## Build the Docker container used for cross-compilation
	@echo "🐳 Building cross-compilation Docker image..."
	DOCKER_BUILDKIT=1 docker build -t $(DOCKER_IMG) -f Dockerfile.cross .

docker-build: docker-image ## Cross-compile for ARM64 using Docker (Debug mode)
	@echo "🛠️  Cross-compiling for $(TARGET_ARCH) (Debug)..."
	$(DOCKER_RUN) bash -c "cargo build --target $(TARGET_ARCH)"

docker-release: docker-image ## Cross-compile for ARM64 using Docker (Release mode)
	@echo "🚀 Cross-compiling for $(TARGET_ARCH) (Release)..."
	$(DOCKER_RUN) bash -c "cargo build --release --target $(TARGET_ARCH)"

docker-check: docker-image ## Fast aarch64 type-check (no codegen) — catches Linux-only breakage
	@echo "🔎 cargo check for $(TARGET_ARCH)..."
	$(DOCKER_RUN) bash -c "cargo check --target $(TARGET_ARCH)"

docker-lint: docker-image ## Clippy for the aarch64 target (lints Linux-only HAL code)
	@echo "🔎 clippy for $(TARGET_ARCH)..."
	$(DOCKER_RUN) bash -c "cargo clippy --target $(TARGET_ARCH) -- -D warnings"

docker-shell: docker-image ## Open an interactive bash shell inside the cross-build container
	@echo "🐚 Launching interactive container shell..."
	$(DOCKER_RUN) bash

docker-clean: ## Wipe the persistent Docker cargo caches (use if dependencies get corrupted)
	@echo "💥 Removing Docker volume caches..."
	docker volume rm $(CARGO_CACHE_VOL) $(CARGO_GIT_VOL) 2>/dev/null || true
	rm -rf $(DOCKER_TARGET_DIR)

# ==============================================================================
# Release Packaging
# ==============================================================================

dist: docker-release ## Build a versioned release tarball (binary + config + systemd unit + sha256)
	@echo "📦 Packaging $(DIST_NAME)..."
	@mkdir -p dist/$(DIST_NAME)/bin dist/$(DIST_NAME)/config
	@cp $(RELEASE_BIN) dist/$(DIST_NAME)/bin/
	@cp config/iiotedge_default.toml dist/$(DIST_NAME)/config/
	@cp deploy/$(APP_NAME).service dist/$(DIST_NAME)/
	@echo "$(VERSION)+$(GIT_HASH)" > dist/$(DIST_NAME)/VERSION
	@tar -czf dist/$(DIST_NAME).tar.gz -C dist $(DIST_NAME)
	@shasum -a 256 dist/$(DIST_NAME).tar.gz | tee dist/$(DIST_NAME).tar.gz.sha256
	@echo "✅ dist/$(DIST_NAME).tar.gz"

DEB_ARCH := arm64

# dpkg-deb is a Debian/Ubuntu tool, not available on macOS by default
# (unlike dist's tarball packaging, which only needs tar/shasum — universal
# POSIX tools) — run inside the same cross-build container docker-release
# already uses, which has it out of the box (Ubuntu base).
deb: docker-release ## Build a .deb (apt/local-repo fleets — see scripts/build-deb.sh)
	$(DOCKER_RUN) bash -c "VERSION=$(VERSION) GIT_HASH=$(GIT_HASH) DEB_ARCH=$(DEB_ARCH) \
		RELEASE_BIN=$(RELEASE_BIN) bash scripts/build-deb.sh"

version: ## Print the firmware version being built
	@echo "$(VERSION)+$(GIT_HASH) ($(TARGET_ARCH))"

# ==============================================================================
# Deployment (device layout: $(DEVICE_DIR)/bin/ + $(DEVICE_DIR)/config/)
# ==============================================================================

deploy: docker-release ## Cross-compile Release and push the binary to the device (restarts service if installed)
	@echo "📦 Deploying $(APP_NAME) $(VERSION)+$(GIT_HASH) to $(DEVICE):$(DEVICE_DIR)..."
	ssh $(DEVICE) 'mkdir -p $(DEVICE_DIR)/bin $(DEVICE_DIR)/config'
	scp $(RELEASE_BIN) $(DEVICE):$(DEVICE_DIR)/bin/$(APP_NAME).new
	ssh $(DEVICE) 'mv $(DEVICE_DIR)/bin/$(APP_NAME).new $(DEVICE_DIR)/bin/$(APP_NAME) && chmod +x $(DEVICE_DIR)/bin/$(APP_NAME)'
	@ssh $(DEVICE) 'systemctl is-active --quiet $(APP_NAME) && sudo systemctl restart $(APP_NAME) && echo "🔄 service restarted" || echo "ℹ️  service not running (start manually or: make install-service)"'
	@echo "✅ Deployment complete."

deploy-config: ## Push config/ (firmware + edge.toml) to the device (overwrites!) and restart service if running
	@echo "⚙️  Pushing config to $(DEVICE):$(DEVICE_DIR)/config/ ..."
	ssh $(DEVICE) 'mkdir -p $(DEVICE_DIR)/config'
	scp config/iiotedge_default.toml config/edge.toml $(DEVICE):$(DEVICE_DIR)/config/
	@ssh $(DEVICE) 'systemctl is-active --quiet $(APP_NAME) && sudo systemctl restart $(APP_NAME) && echo "🔄 service restarted" || true'
	@echo "✅ Config deployed."

install-service: ## Install + enable the systemd unit on the device (survives reboots, restarts on faults)
	@echo "🧷 Installing systemd unit on $(DEVICE)..."
	sed -e 's|@DEVICE_USER@|$(DEVICE_USER)|g' -e 's|@DEVICE_DIR@|$(DEVICE_DIR)|g' \
		deploy/$(APP_NAME).service > /tmp/$(APP_NAME).service.rendered
	scp /tmp/$(APP_NAME).service.rendered $(DEVICE):/tmp/$(APP_NAME).service
	ssh -t $(DEVICE) 'sudo mv /tmp/$(APP_NAME).service /etc/systemd/system/$(APP_NAME).service && sudo systemctl daemon-reload && sudo systemctl enable --now $(APP_NAME)'
	@rm -f /tmp/$(APP_NAME).service.rendered
	@echo "✅ Service installed and started. Follow with: make device-logs"

# ==============================================================================
# Remote Operations
# ==============================================================================

device-status: ## Show firmware service status on the device
	ssh $(DEVICE) 'systemctl status $(APP_NAME) --no-pager -l | head -20'

device-logs: ## Tail live firmware logs from the device (Ctrl+C to stop)
	ssh $(DEVICE) 'journalctl -u $(APP_NAME) -f -n 50'

device-restart: ## Restart the firmware service on the device
	ssh $(DEVICE) 'sudo systemctl restart $(APP_NAME)' && echo "🔄 restarted"

device-shell: ## SSH into the device
	ssh $(DEVICE)

# ==============================================================================
# Utility
# ==============================================================================

help: ## Show this help message
	@echo "Fusion Firmware Makefile — version $(VERSION)+$(GIT_HASH)"
	@echo "Device: $(DEVICE):$(DEVICE_DIR)  (override with DEVICE_IP=/DEVICE_USER=/DEVICE_DIR=)"
	@echo ""
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'
