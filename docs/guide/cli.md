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
| `info <path>` | File length, and each branch with its version, commit count and last commit time. |
| `log <path> [--branch main] [--limit 20]` | A branch's commits, newest first, one line each. |
| `verify <path>` | Re-hashes every reachable object, checks the branch table's map, and checks that each branch's merge records name a branch the file holds, as `Db::verify` does. It cannot decode your types, which `Db::verify_typed` does. |
| `space <path>` | File length, the bytes the log takes, and an estimate of the live bytes. |
| `compact <path> [--keep-last N \| --since-ms T]` | Rewrites the file, keeping the `N` newest commits per branch, or those at or after unix time `T` in milliseconds. |

`log` prints each commit as `v<version>  <unix ms>  <messages>`. A commit that `Options::coalesce`
made of several closures has their messages joined by `; `, and control characters and backslashes
in a message are escaped as Rust escapes them, `\n` for a newline and `\\` for a backslash, so
a message cannot break a line or forge one.

The file length that `info`, `space` and `verify` print includes the zeroes a writer preallocates
past the log, up to 4 MiB, and the staging frames of a receive of more than 1 MiB that end the log,
while it has the file open, or after shared writers closed it, until the next writer opens it.
`space` prints the log's bytes beside it. `compact` prints the file's length
before and after.

Output goes to standard output and errors to standard error, as `ferro: <error>` with exit status 1.
Output into a pipe that closed, as `ferro log shop.ferro | head` closes it, ends the command with
status 0.

`compact` copies what is reachable to a new file and swaps it in atomically, on as many threads as a
reclamation cycle takes by default, the cores up to 8, and fails while a writer has the file open.
Like the other commands, it refuses a file that is not a database, an empty one included, and
leaves it as it was. See [Retention and reclamation](retention.md) for doing this from a running
program.

The `Makefile` wraps each command with `DB=path`: `make info`, `make log ARGS='--limit 50'`,
`make verify`, `make space`, and `make compact KEEP=1000`.
