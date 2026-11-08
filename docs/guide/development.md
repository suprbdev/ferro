# Development

The Makefile wraps the common commands, and `make help` lists every target. `make ci` checks
formatting, runs clippy for Linux and Windows and on the fuzz crate, builds the API docs, and then
runs every test and the library's unit tests in parallel copies. `make test-one T=sync` runs the
library tests whose names match, and `make bench-tables` times booking-style commits. The examples
and the `ferro` tool take their paths from variables:

```sh
make shop DB=target/demo/shop.ferro ADDR=127.0.0.1:3000
make info DB=target/demo/shop.ferro
```

Cargo runs inside a `systemd-run` scope capped at `MEM` (16G by default), so a runaway test cannot
take the machine down. Set `CAP=` to run without the cap. Cargo runs `JOBS` compile jobs at once,
one for each GiB of `MEM` by default, since linking the test binaries on every core of a large
machine passes the cap; set `JOBS` when `MEM` is not given in G.

| Target | Does |
| --- | --- |
| `make build`, `make build-debug` | Build the workspace in release or debug. |
| `make check` | Type-check every target. |
| `make test` | Every test in the workspace, in release. |
| `make test-lib` | The library's tests only. |
| `make test-one T=filter` | Library tests whose names match `T`. |
| `make test-pkg P=booking T=filter` | One package's tests. |
| `make test-stress` | The library's unit tests in `STRESS_COPIES` processes at once, `STRESS_ROUNDS` times over, 12 and 25 by default, as [How the protocol is checked](../internals/read-scaling.md#how-the-protocol-is-checked) describes. |
| `make lint`, `make lint-windows` | clippy with warnings as errors, for Linux and for `x86_64-pc-windows-gnu`, with `undocumented_unsafe_blocks` and `missing_safety_doc` on. |
| `make ci` | `fmt-check`, `lint`, `lint-windows`, `fuzz-lint`, `doc-check`, `test` and `test-stress`, the local equivalent of CI. |
| `make act` | The GitHub workflow's Linux job, locally with `act`. |
| `make fmt`, `make fmt-check` | Format the workspace with rustfmt, or fail if any file is not formatted. |
| `make doc`, `make doc-check` | The library's API docs, or a build of them that fails on any rustdoc warning, such as a broken link. |
| `make bench` | The main benchmark, appending to `target/bench/results.jsonl`. |
| `make bench-reads`, `bench-scaling`, `bench-heap`, `bench-tables`, `bench-bulk`, `bench-merge`, `bench-covered`, `bench-combine` | The `ferro-bench` tools of those names. |
| `make bench-compare` | ferro against SQLite, redb and fjall. |
| `make fuzz-seeds`, `make fuzz FUZZ=open` | Fuzzing, as described in [Fuzzing and testing](../internals/testing.md). |
| `make fuzz-lint` | rustfmt and clippy on the fuzz crate, which is its own workspace, on stable. |
| `make clean`, `make clean-tmp` | Remove build output, or ferro test directories in `/tmp` untouched for an hour. |

`ferro-bench` has more tools than the `Makefile` names: `collections`, `clone`, `delta`, `follow`,
`group`, `processes`, `reclaim`, `summary` and `watchers` run with `cargo run --release -p
ferro-bench --bin <tool>`.
`make ARGS='...'` passes arguments through to any of them.

CI runs clippy and every test on Linux, and clippy and the library's tests on `windows-latest`.
