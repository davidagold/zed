SHELL = /bin/zsh

.PHONY: notebook-py-deps notebook

PYTHON_VERSION := 3.12
POETRY_DIR := crates/notebook
RUST_LOG = error

notebook-py-deps:
	poetry install --directory $(POETRY_DIR) --no-root

notebook: notebook-py-deps
	VENV=$(shell poetry --directory $(POETRY_DIR) env info --path) \
	PYTHONPATH=$(shell poetry --directory $(POETRY_DIR) env info --path)/lib/python$(PYTHON_VERSION)/site-packages \
	PYO3_PYTHON=$(shell poetry --directory $(POETRY_DIR) env info --path)/bin/python$(PYTHON_VERSION) \
	PYTHONEXECUTABLE=$(shell poetry --directory $(POETRY_DIR) env info --path)/bin/python$(PYTHON_VERSION) \
	RUST_LOG=$(RUST_LOG) \
    cargo run
