VERSION ?= 0.1.0
ARCH    ?= amd64
DIST    ?= dist

# Find nfpm: PATH first, then the Go bin dirs (go install drops it there).
NFPM ?= $(shell command -v nfpm 2>/dev/null \
	|| command -v "$$(go env GOBIN 2>/dev/null)/nfpm" 2>/dev/null \
	|| command -v "$$(go env GOPATH 2>/dev/null)/bin/nfpm" 2>/dev/null \
	|| echo nfpm)

export VERSION ARCH

.PHONY: all test agent lambda deb rpm packages clean

all: packages

test:
	cargo test

# Release binary for the current host (the agent).
agent:
	cargo build --release -p flotswarm-agent

# Distributor Lambda zip (arm64, provided.al2023).
lambda:
	./scripts/build-lambda.sh

deb: agent | $(DIST)
	cd packaging && $(NFPM) pkg --config nfpm.yaml --packager deb --target ../$(DIST)/

rpm: agent | $(DIST)
	cd packaging && $(NFPM) pkg --config nfpm.yaml --packager rpm --target ../$(DIST)/

packages: deb rpm

$(DIST):
	mkdir -p $(DIST)

clean:
	rm -rf $(DIST) target/lambda
