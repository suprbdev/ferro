CARGO ?= cargo
MEM ?= 16G
JOBS ?= $(patsubst %G,%,$(MEM))
export CARGO_BUILD_JOBS ?= $(JOBS)
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
STRESS_ROUNDS ?= 25
STRESS_COPIES ?= 12
STRESS_THREADS ?= 16
LINTS ?= -W clippy::undocumented_unsafe_blocks -W clippy::missing_safety_doc
GO_IMAGE ?= golang:1.26.9-bookworm@sha256:d9c68c2c51161e12fd77e4c6320687c9cd86e1af1e3ad6e6cd63ff970641453c
VERSIONED = $(CURDIR)/benchmarks/compare/target/versioned
GO_BUILD = docker run --rm --memory=16g --memory-swap=16g --user $$(id -u):$$(id -g) -e HOME=/work/go \
	-e GOPATH=/work/go -e GOCACHE=/work/go/cache -e GOTOOLCHAIN=local -e GOFLAGS=-mod=readonly -v $(VERSIONED):/work

.DEFAULT_GOAL := help
.PHONY: help build build-debug check test test-lib test-one test-pkg test-stress lint lint-windows fmt fmt-check doc doc-check ci act \
	bench bench-reads bench-scaling bench-heap bench-tables bench-bulk bench-merge bench-covered bench-combine bench-compare \
	compare-versioned \
	quickstart tracker shop shop-load chat chat-load notes notes-scale booking booking-stress queue-bench \
	info log verify space compact fuzz fuzz-seeds fuzz-lint clean clean-tmp

help: ## List the targets
	@awk 'BEGIN {FS = ":.*## "} /^[a-zA-Z0-9_-]+:.*## / {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@printf '\nVariables: MEM=%s JOBS=%s T=test-filter P=package DB=path ADDR=%s ARGS="..." FUZZ=%s\n' "$(MEM)" "$(JOBS)" "$(ADDR)" "$(FUZZ)"
	@printf 'STRESS_ROUNDS=%s STRESS_COPIES=%s STRESS_THREADS=%s size test-stress.\n' "$(STRESS_ROUNDS)" "$(STRESS_COPIES)" "$(STRESS_THREADS)"
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

test-stress: ## Run the library's unit tests in STRESS_COPIES processes at once, STRESS_ROUNDS times over
	$(RUN) test --release -p ferro --lib --no-run
	@bin=$$($(CARGO) test --release -p ferro --lib --no-run 2>&1 | sed -n 's|.*Executable unittests src/lib.rs (\(.*\))$$|\1|p'); \
	test -n "$$bin" || { echo "test-stress: found no unit test binary for the library"; exit 1; }; \
	rm -rf target/stress && mkdir -p target/stress && \
	$(CAP) sh -c 'for round in $$(seq "$$1"); do \
		for copy in $$(seq "$$2"); do \
			"$$3" --test-threads "$$4" -q > "target/stress/$$round.$$copy" 2>&1 || echo "$$round.$$copy" >> target/stress/failed & \
		done; \
		wait; \
	done' sh $(STRESS_ROUNDS) $(STRESS_COPIES) "$$bin" $(STRESS_THREADS); \
	runs=$$(($(STRESS_ROUNDS) * $(STRESS_COPIES))); \
	if [ -s target/stress/failed ]; then \
		for run in $$(cat target/stress/failed); do \
			echo "--- run $$run"; grep -E "panicked at|^test .* \.\.\. FAILED|^ *(left|right):|ERROR|WARNING" "target/stress/$$run"; \
		done; \
		echo "test-stress: $$(wc -l < target/stress/failed) of $$runs runs failed, logs in target/stress"; exit 1; \
	fi; \
	echo "test-stress: $$runs runs of the library's unit tests, $(STRESS_COPIES) at a time, all passed"

lint: ## Run clippy on every target with warnings as errors
	$(RUN) clippy --workspace --all-targets --release -- -D warnings $(LINTS)

lint-windows: ## Run clippy for the Windows target
	$(RUN) clippy --target $(WIN_TARGET) --workspace --all-targets --release -- -D warnings $(LINTS)

fmt: ## Format the workspace with rustfmt
	$(CARGO) fmt --all

fmt-check: ## Fail if any file is not formatted with rustfmt
	$(CARGO) fmt --all -- --check

doc: ## Build the library's API docs and open them
	$(CARGO) doc --no-deps -p ferro --open

doc-check: ## Build the API docs with warnings as errors, so broken links fail
	RUSTDOCFLAGS="-D warnings" $(RUN) doc --no-deps -p ferro -p ferro-derive

ci: fmt-check lint lint-windows fuzz-lint doc-check test test-stress ## Run the local equivalent of CI

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

bench-combine: ## Compare ways to combine two index lookups by key
	$(RUN) run --release -p ferro-bench --bin combine -- $(ARGS)

bench-compare: ## Compare ferro with SQLite, redb and fjall
	cd benchmarks/compare && $(RUN) run --release -- $(ARGS)

compare-versioned: ## Compare ferro's history, branches, sync and verify with immudb and Dolt, both in Docker
	@mkdir -p $(VERSIONED)/go $(VERSIONED)/bin $(VERSIONED)/runs
	$(GO_BUILD) -e CGO_ENABLED=0 -v $(CURDIR)/benchmarks/compare/versioned/immudb:/src:ro -w /src $(GO_IMAGE) \
		go build -trimpath -o /work/bin/immudb .
	$(GO_BUILD) -e CGO_ENABLED=1 -v $(CURDIR)/benchmarks/compare/versioned/dolt:/src:ro -w /src $(GO_IMAGE) \
		go build -tags gms_pure_go -trimpath -o /work/bin/dolt .
	cd benchmarks/compare && $(RUN) run --release --bin versioned -- --image $(GO_IMAGE) $(ARGS)

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

info: ## Show a database file's length and branches, DB=path
	$(RUN) run --release -q -p ferro-cli -- info $(DB)

log: ## List a database's newest commits, DB=path
	$(RUN) run --release -q -p ferro-cli -- log $(DB) $(ARGS)

verify: ## Re-hash every reachable object in DB
	$(RUN) run --release -q -p ferro-cli -- verify $(DB)

space: ## Show DB's file length, log bytes and live bytes
	$(RUN) run --release -q -p ferro-cli -- space $(DB)

compact: ## Rewrite DB, keeping KEEP newest commits per branch if set
	$(RUN) run --release -q -p ferro-cli -- compact $(DB) $(if $(KEEP),--keep-last $(KEEP))

fuzz-seeds: ## Build a starting corpus for the fuzz targets
	cd fuzz && $(RUN) run --release --example seeds -- corpus/seeds

fuzz-lint: ## Check the fuzz crate's formatting and run clippy on it with warnings as errors
	cd fuzz && $(CARGO) fmt -- --check
	cd fuzz && $(RUN) clippy --all-targets --release -- -D warnings $(LINTS)

fuzz: ## Fuzz target FUZZ for FUZZ_TIME seconds with nightly
	cd fuzz && $(CAP) cargo +nightly fuzz run $(FUZZ) corpus/seeds/$(FUZZ) -- -max_len=70000 -timeout=10 -malloc_limit_mb=1024 -max_total_time=$(FUZZ_TIME)

clean: ## Remove build output
	$(CARGO) clean

clean-tmp: ## Delete ferro test dirs in /tmp untouched for over an hour
	@find /tmp -maxdepth 2 -path '/tmp/.tmp*' -name '*.ferro' -user $(USER) -mmin +60 -printf '%h\n' | sort -u | while read -r dir; do echo "removing $$dir"; rm -rf -- "$$dir"; done
