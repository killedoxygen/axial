.DEFAULT_GOAL := help
DEV := ./dev

.PHONY: help setup dev dev-web watch build build-dev build-windows build-windows-dev check test verify clean doctor

help:
	@$(DEV) help

setup:
	@$(DEV) setup

dev:
	@$(DEV) dev

dev-web:
	@$(DEV) dev-web

watch:
	@$(DEV) watch

build:
	@$(DEV) build

build-dev:
	@$(DEV) build-dev

build-windows:
	@$(DEV) build:windows

build-windows-dev:
	@$(DEV) build:windows:dev

check:
	@$(DEV) check

test:
	@$(DEV) test

verify:
	@$(DEV) verify

clean:
	@$(DEV) clean

doctor:
	@$(DEV) doctor
