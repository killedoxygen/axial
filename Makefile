.DEFAULT_GOAL := help
DEV := ./dev

.PHONY: help setup dev dev-web dev-windows watch build build-dev build-windows build-windows-dev check test verify clean release-snapshot doctor

help:
	@$(DEV) help

setup:
	@$(DEV) setup

dev:
	@$(DEV) dev

dev-web:
	@$(DEV) dev-web

dev-windows:
	@$(DEV) dev-windows

watch:
	@$(DEV) watch

build:
	@$(DEV) build

build-dev:
	@$(DEV) build-dev

build-windows:
	@$(DEV) build-windows

build-windows-dev:
	@$(DEV) build-windows-dev

check:
	@$(DEV) check

test:
	@$(DEV) test

verify:
	@$(DEV) verify

clean:
	@$(DEV) clean

release-snapshot:
	@$(DEV) release-snapshot

doctor:
	@$(DEV) doctor
