.DEFAULT_GOAL := help
TASK ?= $(or $(shell command -v task 2>/dev/null),$(HOME)/go/bin/task)

.PHONY: _require-task help setup frontend-install frontend-check frontend-build check test build build-dev build-windows build-windows-dev dev serve wails-build verify clean release-snapshot

_require-task:
	@[ -x "$(TASK)" ] || { \
		echo "task is required."; \
		echo ""; \
		echo "install it with:"; \
		echo "  go install github.com/go-task/task/v3/cmd/task@latest"; \
		echo ""; \
		echo "then either add ~/go/bin to PATH or rerun make."; \
		exit 1; \
	}

help:
	@if [ -x "$(TASK)" ]; then \
		$(TASK) --list-all; \
	else \
		echo "task is required."; \
		echo ""; \
		echo "install it with:"; \
		echo "  go install github.com/go-task/task/v3/cmd/task@latest"; \
		echo ""; \
		echo "common commands after that:"; \
		echo "  make setup"; \
		echo "  make dev"; \
		echo "  make build"; \
		echo "  make build-dev"; \
		echo "  make verify"; \
	fi

setup: _require-task
	@$(TASK) frontend:install

frontend-install: _require-task
	@$(TASK) frontend:install

frontend-check: _require-task
	@$(TASK) frontend:check

frontend-build: _require-task
	@$(TASK) frontend:build

check: _require-task
	@$(TASK) check

test: _require-task
	@$(TASK) test

build: _require-task
	@$(TASK) build

build-dev: _require-task
	@$(TASK) build:dev

build-windows: _require-task
	@$(TASK) build:windows

build-windows-dev: _require-task
	@$(TASK) build:windows:dev

dev: _require-task
	@$(TASK) wails:dev

serve: _require-task
	@$(TASK) frontend:serve

wails-build: _require-task
	@$(TASK) wails:build

verify: _require-task
	@$(TASK) verify

clean: _require-task
	@$(TASK) clean

release-snapshot: _require-task
	@$(TASK) release:snapshot
