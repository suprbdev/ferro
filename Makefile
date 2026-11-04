CARGO ?= cargo
MEM ?= 16G
CAP ?= $(if $(shell command -v systemd-run 2>/dev/null),systemd-run --user --scope -q -p MemoryMax=$(MEM) -p MemorySwapMax=0)
RUN = $(CAP) $(CARGO)
WIN_TARGET ?= x86_64-pc-windows-gnu
T ?=
P ?= ferro
ARGS ?=
DB ?= target/demo/demo.ferro
ADDR ?= 127.0.0.1:3000
SECS ?= 10
KEEP ?=
FUZZ ?= open
FUZZ_TIME ?= 600

.DEFAULT_GOAL := help
.PHONY: help build build-debug check test test-lib test-one test-pkg lint lint-windows fmt fmt-check doc ci act \
	bench bench-reads bench-scaling bench-heap bench-tables bench-bulk bench-merge bench-covered bench-compare \
	quickstart tracker shop shop-load chat chat-load notes notes-scale booking booking-stress queue-bench \
	info log verify space compact fuzz fuzz-seeds clean clean-tmp

help: ## List the targets
	@awk 'BEGIN {FS = ":.*## "} /^[a-zA-Z0-9_-]+:.*## / {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@printf '\nVariables: MEM=%s T=test-filter P=package DB=path ADDR=%s ARGS="..." FUZZ=%s\n' "$(MEM)" "$(ADDR)" "$(FUZZ)"
	@printf 'Set CAP= (empty) to run cargo without the systemd memory cap.\n'

build: ## Build the whole workspace in release
	$(RUN) build --release --workspace

build-debug: ## Build the whole workspace in debug
	$(RUN) build --workspace

check: ## Type-check every target in the workspace
	$(RUN) check --workspace --all-targets

test: ## Run every test in the workspace in release
	$(RUN) test --release --workspace --no-fail-fast

test-lib: ## Run only the library's tests
	$(RUN) test --release -p ferro

test-one: ## Run tests matching T in the library, e.g. make test-one T=sync
	$(RUN) test --release -p ferro -- $(T)

test-pkg: ## Run one package's tests, e.g. make test-pkg P=booking
	$(RUN) test --release -p $(P) -- $(T)

lint: ## Run clippy on every target with warnings as errors
	$(RUN) clippy --workspace --all-targets --release -- -D warnings

lint-windows: ## Run clippy for the Windows target
	$(RUN) clippy --target $(WIN_TARGET) --workspace --all-targets --release -- -D warnings

fmt: ## Format the workspace with rustfmt
	$(CARGO) fmt --all

fmt-check: ## Fail if any file is not formatted with rustfmt
	$(CARGO) fmt --all -- --check

doc: ## Build the library's API docs and open them
	$(CARGO) doc --no-deps -p ferro --open

ci: fmt-check lint lint-windows test ## Run the local equivalent of CI

act: ## Run the GitHub workflow's Linux job locally with act
	act push -j test

bench: ## Run the main benchmark and append to target/bench/results.jsonl
	$(RUN) run --release -p ferro-bench -- $(ARGS)

bench-reads: ## Benchmark reads
	$(RUN) run --release -p ferro-bench --bin reads -- $(ARGS)

bench-scaling: ## Measure reads from 1 to 32 threads on shared snapshots
	$(RUN) run --release -p ferro-bench --bin scaling -- $(ARGS)

bench-heap: ## Measure memory held by the cache and working trees
	$(RUN) run --release -p ferro-bench --bin heap -- $(ARGS)

bench-tables: ## Benchmark booking-style multi-table commits
	$(RUN) run --release -p ferro-bench --bin tables -- $(ARGS)

bench-bulk: ## Benchmark a bulk load phase by phase
	$(RUN) run --release -p ferro-bench --bin bulk -- $(ARGS)

bench-merge: ## Benchmark merge-base searches
	$(RUN) run --release -p ferro-bench --bin merge -- $(ARGS)

bench-covered: ## Measure index size and commit cost with and without covered fields
	$(RUN) run --release -p ferro-bench --bin covered -- $(ARGS)

bench-compare: ## Compare ferro with SQLite, redb and fjall
	cd benchmarks/compare && $(RUN) run --release -- $(ARGS)

quickstart: ## Run the library's quickstart example
	$(RUN) run --release -p ferro --example quickstart

tracker: ## Run the issue tracker CLI, e.g. make tracker ARGS='list'
	$(RUN) run --release -p tracker -- $(ARGS)

shop: ## Serve the shop example on ADDR with database DB
	@mkdir -p $(dir $(DB))
	$(RUN) run --release -p shop --bin shop -- --db $(DB) --addr $(ADDR) $(ARGS)

shop-load: ## Load-test a running shop server on ADDR for SECS seconds
	$(RUN) run --release -p shop --bin shop-load -- run --addr $(ADDR) --secs $(SECS) --seed $(ARGS)

chat: ## Serve the chat example on ADDR with database DB
	@mkdir -p $(dir $(DB))
	$(RUN) run --release -p chat --bin chat -- --db $(DB) --addr $(ADDR) $(ARGS)

chat-load: ## Load-test chat against a fresh database for SECS seconds
	@mkdir -p target/demo
	$(RUN) run --release -p chat --bin chat-load -- --db target/demo/chat-load.ferro --seconds $(SECS) $(ARGS)

notes: ## Run the notes CLI, e.g. make notes ARGS='--device a add hello'
	$(RUN) run --release -p notes --bin notes -- $(ARGS)

notes-scale: ## Measure syncing two devices with 100k notes
	$(RUN) run --release -p notes --bin notes-scale -- $(ARGS)

booking: ## Run the booking CLI, e.g. make booking ARGS='seed'
	$(RUN) run --release -p booking --bin booking -- $(ARGS)

booking-stress: ## Race threads for rooms and check every version
	$(RUN) run --release -p booking --bin booking-stress -- $(ARGS)

queue-bench: ## Benchmark the durable job queue
	$(RUN) run --release -p queue --bin queue-bench -- $(ARGS)

info: ## Show a database file's size and branches, DB=path
	$(RUN) run --release -q -p ferro-cli -- info $(DB)

log: ## List a database's newest commits, DB=path
	$(RUN) run --release -q -p ferro-cli -- log $(DB) $(ARGS)

verify: ## Re-hash every reachable object in DB
	$(RUN) run --release -q -p ferro-cli -- verify $(DB)

space: ## Show DB's size against its live bytes
	$(RUN) run --release -q -p ferro-cli -- space $(DB)

compact: ## Rewrite DB, keeping KEEP newest commits per branch if set
	$(RUN) run --release -q -p ferro-cli -- compact $(DB) $(if $(KEEP),--keep-last $(KEEP))

fuzz-seeds: ## Build a starting corpus for the fuzz targets
	cd fuzz && $(RUN) run --release --example seeds -- corpus/seeds

fuzz: ## Fuzz target FUZZ for FUZZ_TIME seconds with nightly
	cd fuzz && $(CAP) cargo +nightly fuzz run $(FUZZ) corpus/seeds/$(FUZZ) -- -max_len=70000 -timeout=10 -malloc_limit_mb=1024 -max_total_time=$(FUZZ_TIME)

clean: ## Remove build output
	$(CARGO) clean

clean-tmp: ## Delete ferro test dirs in /tmp untouched for over an hour
	@find /tmp -maxdepth 2 -path '/tmp/.tmp*' -name '*.ferro' -user $(USER) -mmin +60 -printf '%h\n' | sort -u | while read -r dir; do echo "removing $$dir"; rm -rf -- "$$dir"; done
