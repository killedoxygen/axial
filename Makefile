SHELL := /bin/bash

GO ?= go
NPM ?= npm
WAILS ?= wails
FRONTEND_DIR := frontend
BIN_DIR := build/bin
BIN := $(BIN_DIR)/croopor
GO_BUILD_TAGS ?= webkit2_41
GO_CACHE_DIR ?= $(CURDIR)/.cache/go-build
GO_ENV = GOCACHE=$(GO_CACHE_DIR)

.PHONY: frontend-install frontend-check frontend-build fmt-check check test build build-dev build-dev-windows dev serve wails-build verify clean

frontend-install:
	cd $(FRONTEND_DIR) && $(NPM) ci

frontend-check:
	cd $(FRONTEND_DIR) && $(NPM) run check

frontend-build:
	cd $(FRONTEND_DIR) && $(NPM) run build

fmt-check:
	@out="$$(gofmt -l $$(find . -type f -name '*.go' -not -path './build/*' -not -path './vendor/*'))"; \
	if [ -n "$$out" ]; then \
		echo "$$out"; \
		exit 1; \
	fi

check: frontend-check fmt-check

test:
	mkdir -p $(GO_CACHE_DIR)
	$(GO_ENV) $(GO) test ./...

build: frontend-build
	mkdir -p $(BIN_DIR)
	mkdir -p $(GO_CACHE_DIR)
	$(GO_ENV) $(GO) build -trimpath -tags $(GO_BUILD_TAGS) -o $(BIN) .

build-dev: frontend-build
	mkdir -p $(BIN_DIR)
	mkdir -p $(GO_CACHE_DIR)
	$(GO_ENV) $(GO) build -trimpath -tags dev -o $(BIN_DIR)/croopor-dev .

build-dev-windows: frontend-build
	mkdir -p $(BIN_DIR)
	mkdir -p $(GO_CACHE_DIR)
	$(GO_ENV) GOOS=windows GOARCH=amd64 $(GO) build -trimpath -tags dev -o $(BIN_DIR)/croopor-dev.exe .

dev:
	$(WAILS) dev

serve:
	cd $(FRONTEND_DIR) && $(NPM) run dev

wails-build: frontend-build
	mkdir -p $(GO_CACHE_DIR)
	$(GO_ENV) $(WAILS) build -nopackage -m -v 1

verify: check test build wails-build

clean:
	rm -rf $(BIN_DIR)
