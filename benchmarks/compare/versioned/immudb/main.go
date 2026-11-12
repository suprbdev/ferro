package main

import (
	"bytes"
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/codenotary/immudb/embedded/logger"
	"github.com/codenotary/immudb/embedded/store"
)

const (
	keys         = 100000
	batch        = 1000
	commits      = 1000
	gets         = 20000
	ranges       = 1000
	rangeLen     = 100
	verifiedGets = 1000
)

func mix(x uint64) uint64 {
	x += 0x9e3779b97f4a7c15
	x = (x ^ (x >> 30)) * 0xbf58476d1ce4e5b9
	x = (x ^ (x >> 27)) * 0x94d049bb133111eb
	return x ^ (x >> 31)
}

func key(i uint64) uint64        { return mix(i) }
func value(i uint64) string      { return fmt.Sprintf("value-%d", i) }
func pick(j uint64) uint64       { return mix(j^(1<<50)) % keys }
func rangeStart(j uint64) uint64 { return mix(j^(1<<51)) >> 1 }
func updated(i uint64) uint64    { return mix(i^(1<<52)) % keys }
func buffered(i uint64) uint64   { return mix(i^(1<<53)) % keys }

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

func be(k uint64) []byte {
	b := make([]byte, 8)
	binary.BigEndian.PutUint64(b, k)
	return b
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
	ctx context.Context
	dir string
	st  *store.ImmuStore
}

func (b *bench) open(synced bool) {
	opts := store.DefaultOptions().
		WithSynced(synced).
		WithSyncFrequency(0).
		WithLogger(logger.NewSimpleLoggerWithLevel("immudb ", os.Stderr, logger.LogError))
	st, err := store.Open(b.dir, opts)
	must(err)
	b.st = st
}

func (b *bench) close() {
	must(b.st.Close())
	b.st = nil
}

func (b *bench) put(ks []uint64, vs []string) uint64 {
	tx, err := b.st.NewWriteOnlyTx(b.ctx)
	must(err)
	for i, k := range ks {
		must(tx.Set(be(k), nil, []byte(vs[i])))
	}
	hdr, err := tx.Commit(b.ctx)
	must(err)
	return hdr.ID
}

func (b *bench) get(k uint64) []byte {
	ref, err := b.st.Get(b.ctx, be(k))
	must(err)
	v, err := ref.Resolve()
	must(err)
	return v
}

func (b *bench) getAt(k, version uint64) ([]byte, bool) {
	ref, err := b.st.GetBetween(b.ctx, be(k), 1, version)
	if errors.Is(err, store.ErrKeyNotFound) {
		return nil, false
	}
	must(err)
	v, err := ref.Resolve()
	must(err)
	return v, true
}

func (b *bench) ranges(version uint64) int {
	snap, err := b.st.Snapshot(nil)
	must(err)
	defer snap.Close()
	total := 0
	for j := uint64(0); j < ranges; j++ {
		r, err := snap.NewKeyReader(store.KeyReaderSpec{SeekKey: be(rangeStart(j)), InclusiveSeek: true})
		must(err)
		for n := 0; n < rangeLen; n++ {
			var ref store.ValueRef
			if version == 0 {
				_, ref, err = r.Read(b.ctx)
			} else {
				_, ref, err = r.ReadBetween(b.ctx, 1, version)
			}
			if errors.Is(err, store.ErrNoMoreEntries) {
				break
			}
			must(err)
			_, err = ref.Resolve()
			must(err)
			total++
		}
		must(r.Close())
	}
	return total
}

func (b *bench) diff(from, to uint64) int {
	tx := store.NewTx(b.st.MaxTxEntries(), b.st.MaxKeyLen())
	touched := map[string]struct{}{}
	for id := from + 1; id <= to; id++ {
		must(b.st.ReadTx(id, false, tx))
		for _, e := range tx.Entries() {
			touched[string(e.Key())] = struct{}{}
		}
	}
	sorted := make([]string, 0, len(touched))
	for k := range touched {
		sorted = append(sorted, k)
	}
	sort.Strings(sorted)
	changes := 0
	for _, k := range sorted {
		k := binary.BigEndian.Uint64([]byte(k))
		before, hadBefore := b.getAt(k, from)
		after, hasAfter := b.getAt(k, to)
		if hadBefore != hasAfter || !bytes.Equal(before, after) {
			changes++
		}
	}
	return changes
}

func (b *bench) verifyAll() int {
	tx := store.NewTx(b.st.MaxTxEntries(), b.st.MaxKeyLen())
	last := b.st.LastCommittedTxID()
	r, err := b.st.NewTxReader(1, false, tx)
	must(err)
	entries := 0
	for id := uint64(1); id <= last; id++ {
		t, err := r.Read()
		must(err)
		for _, e := range t.Entries() {
			_, err := b.st.ReadValue(e)
			must(err)
			entries++
		}
	}
	return entries
}

func (b *bench) verifiedGet(k uint64, target *store.TxHeader, tx *store.Tx) {
	kb := be(k)
	ref, err := b.st.Get(b.ctx, kb)
	must(err)
	must(b.st.ReadTx(ref.Tx(), false, tx))
	e, err := tx.EntryOf(kb)
	must(err)
	_, err = b.st.ReadValue(e)
	must(err)
	proof, err := tx.Proof(kb)
	must(err)
	hdr := tx.Header()
	digest, err := hdr.TxEntryDigest()
	must(err)
	d, err := digest(e)
	must(err)
	if !store.VerifyInclusion(proof, d, hdr.Eh) {
		panic("inclusion proof failed")
	}
	dual, err := b.st.DualProof(hdr, target)
	must(err)
	if !store.VerifyDualProof(dual, hdr.ID, target.ID, hdr.Alh(), target.Alh()) {
		panic("dual proof failed")
	}
}

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: immudb <dir>")
		os.Exit(2)
	}
	b := &bench{ctx: context.Background(), dir: filepath.Join(os.Args[1], "db")}
	must(os.MkdirAll(b.dir, 0o755))
	out := map[string]float64{}

	b.open(true)
	var lat []float64
	start := time.Now()
	var v0 uint64
	for c := uint64(0); c < keys/batch; c++ {
		ks := make([]uint64, 0, batch)
		vs := make([]string, 0, batch)
		for i := c * batch; i < (c+1)*batch; i++ {
			ks = append(ks, key(i))
			vs = append(vs, value(i))
		}
		t := time.Now()
		v0 = b.put(ks, vs)
		lat = append(lat, ms(time.Since(t)))
	}
	out["load_s"] = time.Since(start).Seconds()
	out["batch_ms"] = quantile(lat, 0.5)
	b.close()
	sizeLoad := dirSize(b.dir)
	out["size_load_mib"] = sizeLoad / 1048576

	t := time.Now()
	b.open(true)
	b.get(key(0))
	out["reopen_ms"] = ms(time.Since(t))

	lat = lat[:0]
	var v1 uint64
	for i := uint64(0); i < commits; i++ {
		t := time.Now()
		v1 = b.put([]uint64{key(updated(i))}, []string{fmt.Sprintf("upd-%d", i)})
		lat = append(lat, ms(time.Since(t)))
	}
	out["commit_p50_ms"] = quantile(lat, 0.5)
	out["commit_p99_ms"] = quantile(lat, 0.99)
	out["commit_mean_ms"] = mean(lat)
	b.close()
	out["commit_bytes"] = (dirSize(b.dir) - sizeLoad) / commits

	b.open(false)
	lat = lat[:0]
	for i := uint64(0); i < commits; i++ {
		t := time.Now()
		b.put([]uint64{key(buffered(i))}, []string{fmt.Sprintf("buf-%d", i)})
		lat = append(lat, ms(time.Since(t)))
	}
	out["buffered_p50_ms"] = quantile(lat, 0.5)
	out["buffered_mean_ms"] = mean(lat)
	b.close()

	b.open(true)
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
		if got := b.ranges(0); got < ranges*rangeLen*99/100 {
			panic(fmt.Sprintf("ranges read %d", got))
		}
		elapsed = time.Since(t)
	}
	out["range_us"] = float64(elapsed.Nanoseconds()) / 1e3 / ranges

	for pass := 0; pass < 2; pass++ {
		t := time.Now()
		for j := uint64(0); j < gets; j++ {
			i := pick(j)
			v, ok := b.getAt(key(i), v0)
			if !ok || string(v) != value(i) {
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
	changes := b.diff(v0, v1)
	out["diff_ms"] = ms(time.Since(t))
	if want := distinct(commits, updated); changes != want {
		panic(fmt.Sprintf("diff found %d changes, want %d", changes, want))
	}
	out["diff_changes"] = float64(changes)

	t = time.Now()
	entries := b.verifyAll()
	out["verify_ms"] = ms(time.Since(t))
	out["verify_items"] = float64(entries)

	target, err := b.st.ReadTxHeader(b.st.LastCommittedTxID(), false, false)
	must(err)
	tx := store.NewTx(b.st.MaxTxEntries(), b.st.MaxKeyLen())
	t = time.Now()
	for j := uint64(0); j < verifiedGets; j++ {
		b.verifiedGet(key(pick(j)), target, tx)
	}
	out["verified_get_us"] = float64(time.Since(t).Nanoseconds()) / 1e3 / verifiedGets
	b.close()

	b.open(true)
	b.get(key(0))
	t = time.Now()
	b.put([]uint64{key(keys - 1)}, []string{"first"})
	out["first_commit_ms"] = ms(time.Since(t))
	b.close()
	out["size_end_mib"] = dirSize(b.dir) / 1048576

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
