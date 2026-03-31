.DEFAULT_GOAL := help
TASK ?= task

.PHONY: _require-task help frontend-install frontend-check frontend-build check test build build-dev build-windows build-windows-dev dev serve wails-build verify clean release-snapshot

_require-task:
	@$(TASK) --version >/dev/null 2>&1 || { \
		echo "task is required. install it with:"; \
		echo "  go install github.com/go-task/task/v3/cmd/task@latest"; \
		exit 1; \
	}

help: _require-task
	@$(TASK) --list-all

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
