# The `ferro` tool

`ferro` works on any database file without your types:

```sh
cargo run --release -p ferro-cli -- info shop.ferro
cargo run --release -p ferro-cli -- log shop.ferro --limit 20
cargo run --release -p ferro-cli -- verify shop.ferro
cargo run --release -p ferro-cli -- space shop.ferro
cargo run --release -p ferro-cli -- compact shop.ferro --keep-last 1000
```

| Command | Does |
| --- | --- |
| `info <path>` | File size, and each branch with its version, commit count and last commit time. |
| `log <path> [--branch main] [--limit 20]` | A branch's commits, newest first. |
| `verify <path>` | Re-hashes every reachable object. |
| `space <path>` | File size against estimated live bytes. |
| `compact <path> [--keep-last N \| --since-ms T]` | Rewrites the file, keeping the `N` newest commits per branch, or those at or after unix time `T` in milliseconds. |

`compact` copies what is reachable to a new file and swaps it in atomically, on as many threads as a
reclamation cycle takes by default, the cores up to 8, and fails while a writer has the file open.
See [Retention and reclamation](retention.md) for doing this from a running program.

The `Makefile` wraps each command with `DB=path`: `make info`, `make log ARGS='--limit 50'`,
`make verify`, `make space`, and `make compact KEEP=1000`.
