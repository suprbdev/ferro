package main

import (
	"context"
	"database/sql"
	"fmt"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/dolthub/dolt/go/cmd/dolt/commands"
	"github.com/dolthub/dolt/go/libraries/doltcore/env"
	"github.com/dolthub/dolt/go/libraries/utils/filesys"
	_ "github.com/dolthub/driver"
)

const (
	keys         = 100000
	batch        = 1000
	commits      = 1000
	gets         = 20000
	ranges       = 1000
	rangeLen     = 100
	mergeCommits = 100
	pullCommits  = 100
)

func mix(x uint64) uint64 {
	x += 0x9e3779b97f4a7c15
	x = (x ^ (x >> 30)) * 0xbf58476d1ce4e5b9
	x = (x ^ (x >> 27)) * 0x94d049bb133111eb
	return x ^ (x >> 31)
}

func key(i uint64) int64        { return int64(mix(i) ^ (1 << 63)) }
func value(i uint64) string     { return fmt.Sprintf("value-%d", i) }
func pick(j uint64) uint64      { return mix(j^(1<<50)) % keys }
func rangeStart(j uint64) int64 { return int64((mix(j^(1<<51)) >> 1) ^ (1 << 63)) }
func updated(i uint64) uint64   { return mix(i^(1<<52)) % keys }
func buffered(i uint64) uint64  { return mix(i^(1<<53)) % keys }
func pulled(i uint64) uint64    { return mix(i^(1<<54)) % keys }

func distinct(n uint64, f func(uint64) uint64) int {
	seen := map[uint64]struct{}{}
	for i := uint64(0); i < n; i++ {
		seen[f(i)] = struct{}{}
	}
	return len(seen)
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func dirSize(dir string) float64 {
	var total int64
	must(filepath.WalkDir(dir, func(_ string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if !d.IsDir() {
			info, err := d.Info()
			if err != nil {
				return err
			}
			total += info.Size()
		}
		return nil
	}))
	return float64(total)
}

func ms(d time.Duration) float64 { return float64(d.Nanoseconds()) / 1e6 }

func quantile(v []float64, q float64) float64 {
	s := append([]float64(nil), v...)
	sort.Float64s(s)
	return s[int(q*float64(len(s)-1))]
}

func mean(v []float64) float64 {
	t := 0.0
	for _, x := range v {
		t += x
	}
	return t / float64(len(v))
}

type bench struct {
	ctx  context.Context
	root string
	pool *sql.DB
	db   *sql.Conn
}

func (b *bench) dsn(database string) string {
	d := "file://" + b.root + "?commitname=bench&commitemail=bench@example.com"
	if database != "" {
		d += "&database=" + database
	}
	return d
}

func (b *bench) open(database string) {
	db, err := sql.Open("dolt", b.dsn(database))
	must(err)
	db.SetMaxOpenConns(1)
	conn, err := db.Conn(b.ctx)
	must(err)
	b.pool = db
	b.db = conn
}

func (b *bench) close() {
	must(b.db.Close())
	must(b.pool.Close())
	b.db = nil
	b.pool = nil
}

func (b *bench) exec(q string, args ...any) {
	_, err := b.db.ExecContext(b.ctx, q, args...)
	must(err)
}

func (b *bench) commit(q string, args ...any) string {
	tx, err := b.db.BeginTx(b.ctx, nil)
	must(err)
	_, err = tx.Exec(q, args...)
	must(err)
	var h string
	must(tx.QueryRow("CALL DOLT_COMMIT('-am', 'c')").Scan(&h))
	must(tx.Commit())
	return h
}

func (b *bench) get(k int64) string {
	var v string
	must(b.db.QueryRowContext(b.ctx, "SELECT v FROM t WHERE k = ?", k).Scan(&v))
	return v
}

func (b *bench) ranges(asOf string) int {
	q := "SELECT k, v FROM t WHERE k >= ? ORDER BY k LIMIT ?"
	if asOf != "" {
		q = "SELECT k, v FROM t AS OF '" + asOf + "' WHERE k >= ? ORDER BY k LIMIT ?"
	}
	total := 0
	for j := uint64(0); j < ranges; j++ {
		rows, err := b.db.QueryContext(b.ctx, q, rangeStart(j), rangeLen)
		must(err)
		for rows.Next() {
			var k int64
			var v string
			must(rows.Scan(&k, &v))
			total++
		}
		must(rows.Err())
		must(rows.Close())
	}
	return total
}

func fsck(dir string) {
	fsys, err := filesys.LocalFilesysWithWorkingDir(dir)
	must(err)
	t := time.Now()
	code := commands.FsckCmd{}.Exec(context.Background(), "fsck", []string{"--quiet"}, &env.DoltEnv{FS: fsys}, nil)
	if code != 0 {
		panic(fmt.Sprintf("fsck exited %d", code))
	}
	fmt.Printf("FSCK %g\n", ms(time.Since(t)))
}

func fsckChild(dir string) float64 {
	cmd := exec.Command(os.Args[0], "--fsck", dir)
	cmd.Stderr = os.Stderr
	text, err := cmd.Output()
	must(err)
	for _, line := range strings.Split(string(text), "\n") {
		if rest, ok := strings.CutPrefix(line, "FSCK "); ok {
			v, err := strconv.ParseFloat(rest, 64)
			must(err)
			return v
		}
	}
	panic("fsck printed no time")
}

func main() {
	if len(os.Args) == 3 && os.Args[1] == "--fsck" {
		fsck(os.Args[2])
		return
	}
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: dolt <dir>")
		os.Exit(2)
	}
	b := &bench{ctx: context.Background(), root: filepath.Join(os.Args[1], "dbs")}
	must(os.MkdirAll(b.root, 0o755))
	dbdir := filepath.Join(b.root, "bench")
	out := map[string]float64{}

	b.open("")
	b.exec("CREATE DATABASE bench")
	b.close()
	b.open("bench")
	b.exec("CREATE TABLE t (k BIGINT PRIMARY KEY, v VARCHAR(64) NOT NULL)")
	b.exec("CALL DOLT_COMMIT('-Am', 'schema')")

	var lat []float64
	start := time.Now()
	var v0 string
	for c := uint64(0); c < keys/batch; c++ {
		var q strings.Builder
		q.WriteString("INSERT INTO t VALUES ")
		for i := c * batch; i < (c+1)*batch; i++ {
			if i > c*batch {
				q.WriteByte(',')
			}
			fmt.Fprintf(&q, "(%d,'%s')", key(i), value(i))
		}
		t := time.Now()
		v0 = b.commit(q.String())
		lat = append(lat, ms(time.Since(t)))
	}
	out["load_s"] = time.Since(start).Seconds()
	out["batch_ms"] = quantile(lat, 0.5)
	b.close()
	sizeLoad := dirSize(dbdir)
	out["size_load_mib"] = sizeLoad / 1048576

	t := time.Now()
	b.open("bench")
	b.get(key(0))
	out["reopen_ms"] = ms(time.Since(t))

	lat = lat[:0]
	var v1 string
	for i := uint64(0); i < commits; i++ {
		t := time.Now()
		v1 = b.commit("REPLACE INTO t VALUES (?, ?)", key(updated(i)), fmt.Sprintf("upd-%d", i))
		lat = append(lat, ms(time.Since(t)))
	}
	out["commit_p50_ms"] = quantile(lat, 0.5)
	out["commit_p99_ms"] = quantile(lat, 0.99)
	out["commit_mean_ms"] = mean(lat)
	b.close()
	out["commit_bytes"] = (dirSize(dbdir) - sizeLoad) / commits

	b.open("bench")
	for i := uint64(0); i < commits; i++ {
		b.commit("REPLACE INTO t VALUES (?, ?)", key(buffered(i)), fmt.Sprintf("buf-%d", i))
	}
	b.close()

	b.open("bench")
	var elapsed time.Duration
	for pass := 0; pass < 2; pass++ {
		t := time.Now()
		for j := uint64(0); j < gets; j++ {
			b.get(key(pick(j)))
		}
		elapsed = time.Since(t)
	}
	out["get_us"] = float64(elapsed.Nanoseconds()) / 1e3 / gets

	for pass := 0; pass < 2; pass++ {
		t := time.Now()
		if got := b.ranges(""); got < ranges*rangeLen*99/100 {
			panic(fmt.Sprintf("ranges read %d", got))
		}
		elapsed = time.Since(t)
	}
	out["range_us"] = float64(elapsed.Nanoseconds()) / 1e3 / ranges

	oldGet := "SELECT v FROM t AS OF '" + v0 + "' WHERE k = ?"
	for pass := 0; pass < 2; pass++ {
		t := time.Now()
		for j := uint64(0); j < gets; j++ {
			i := pick(j)
			var v string
			must(b.db.QueryRowContext(b.ctx, oldGet, key(i)).Scan(&v))
			if v != value(i) {
				panic("old read mismatch")
			}
		}
		elapsed = time.Since(t)
	}
	out["old_get_us"] = float64(elapsed.Nanoseconds()) / 1e3 / gets

	for pass := 0; pass < 2; pass++ {
		t := time.Now()
		if got := b.ranges(v0); got < ranges*rangeLen*99/100 {
			panic(fmt.Sprintf("old ranges read %d", got))
		}
		elapsed = time.Since(t)
	}
	out["old_range_us"] = float64(elapsed.Nanoseconds()) / 1e3 / ranges

	t = time.Now()
	var changes int
	must(b.db.QueryRowContext(b.ctx, "SELECT COUNT(*) FROM DOLT_DIFF('"+v0+"', '"+v1+"', 't')").Scan(&changes))
	out["diff_ms"] = ms(time.Since(t))
	if want := distinct(commits, updated); changes != want {
		panic(fmt.Sprintf("diff found %d changes, want %d", changes, want))
	}
	out["diff_changes"] = float64(changes)
	b.close()

	out["verify_ms"] = fsckChild(dbdir)

	b.open("bench")
	b.get(key(0))
	t = time.Now()
	b.commit("REPLACE INTO t VALUES (?, ?)", key(keys-1), "first")
	out["first_commit_ms"] = ms(time.Since(t))
	t = time.Now()
	b.exec("CALL DOLT_BRANCH('b')")
	out["fork_ms"] = ms(time.Since(t))
	b.exec("USE `bench/b`")
	for j := uint64(0); j < mergeCommits; j++ {
		b.commit("REPLACE INTO t VALUES (?, ?)", key(2*j), fmt.Sprintf("br-%d", j))
	}
	b.exec("USE bench")
	for j := uint64(0); j < mergeCommits; j++ {
		b.commit("REPLACE INTO t VALUES (?, ?)", key(2*j+1), fmt.Sprintf("mn-%d", j))
	}
	t = time.Now()
	var mergeHash string
	var fastForward, conflicts int
	var message string
	must(b.db.QueryRowContext(b.ctx, "CALL DOLT_MERGE('b')").Scan(&mergeHash, &fastForward, &conflicts, &message))
	out["merge_ms"] = ms(time.Since(t))
	if conflicts != 0 || fastForward != 0 {
		panic(fmt.Sprintf("merge: fast_forward=%d conflicts=%d", fastForward, conflicts))
	}
	if b.get(key(0)) != "br-0" || b.get(key(1)) != "mn-0" {
		panic("merge lost a side")
	}
	var parents int
	must(b.db.QueryRowContext(b.ctx, "SELECT COUNT(*) FROM dolt_commit_ancestors WHERE commit_hash = ?", mergeHash).Scan(&parents))
	if parents != 2 {
		panic(fmt.Sprintf("merge commit %q has %d parents: %s", mergeHash, parents, message))
	}

	remote := "file://" + filepath.Join(os.Args[1], "remote")
	b.exec("CALL DOLT_REMOTE('add', 'origin', '" + remote + "')")
	t = time.Now()
	b.exec("CALL DOLT_PUSH('origin', 'main')")
	out["clone_send_ms"] = ms(time.Since(t))
	b.exec("CALL DOLT_CLONE('" + remote + "', 'replica')")
	out["clone_ms"] = ms(time.Since(t))
	cloneBytes := dirSize(filepath.Join(os.Args[1], "remote"))
	out["clone_mib"] = cloneBytes / 1048576

	for j := uint64(0); j < pullCommits; j++ {
		b.commit("REPLACE INTO t VALUES (?, ?)", key(pulled(j)), fmt.Sprintf("pl-%d", j))
	}
	t = time.Now()
	b.exec("CALL DOLT_PUSH('origin', 'main')")
	b.exec("USE replica")
	b.exec("CALL DOLT_PULL('origin', 'main')")
	out["pull_ms"] = ms(time.Since(t))
	out["pull_kib"] = (dirSize(filepath.Join(os.Args[1], "remote")) - cloneBytes) / 1024
	var v string
	must(b.db.QueryRowContext(b.ctx, "SELECT v FROM t WHERE k = ?", key(pulled(pullCommits-1))).Scan(&v))
	if v != fmt.Sprintf("pl-%d", pullCommits-1) {
		panic("pull did not land")
	}
	b.exec("USE bench")
	b.close()
	out["size_end_mib"] = dirSize(dbdir) / 1048576

	names := make([]string, 0, len(out))
	for k := range out {
		names = append(names, k)
	}
	sort.Strings(names)
	parts := make([]string, 0, len(names))
	for _, k := range names {
		parts = append(parts, fmt.Sprintf("%s=%g", k, out[k]))
	}
	fmt.Println("RESULT " + strings.Join(parts, " "))
}
