package load

import (
	"bytes"
	"encoding/json"
	"math"
	"math/rand/v2"
	"slices"
	"strings"
	"testing"
	"time"
)

func TestKeysHaveTheSizeAndSortByIndex(t *testing.T) {
	for _, size := range []int{8, 16, 32} {
		prev := Key(0, size)
		for i := uint64(1); i < 1000; i++ {
			k := Key(i, size)
			if len(k) != size {
				t.Fatalf("Key(%d, %d) has %d bytes", i, size, len(k))
			}
			if bytes.Compare(prev, k) >= 0 {
				t.Fatalf("Key(%d) does not sort after Key(%d) at size %d", i, i-1, size)
			}
			prev = k
		}
	}
	if got := Key(0x0102, 2); !bytes.Equal(got, []byte{1, 2}) {
		t.Fatalf("Key truncates to the low bytes: got %v", got)
	}
}

func TestDistributionsAreDeterministicAndInRange(t *testing.T) {
	for _, spec := range []string{"uniform", "zipf", "zipf:1.5"} {
		a, err := NewDistribution(spec, 1000, 42)
		if err != nil {
			t.Fatal(err)
		}
		b, _ := NewDistribution(spec, 1000, 42)
		for range 10000 {
			x, y := a.Next(), b.Next()
			if x != y {
				t.Fatalf("%s: same seed, different sequence", spec)
			}
			if x >= 1000 {
				t.Fatalf("%s: %d out of range", spec, x)
			}
		}
	}
	for _, bad := range []string{"zipf:1", "zipf:x", "uniform:2", "pareto"} {
		if _, err := NewDistribution(bad, 10, 1); err == nil {
			t.Fatalf("%q was accepted", bad)
		}
	}
}

func TestZipfIsSkewed(t *testing.T) {
	d, _ := NewDistribution("zipf:1.2", 10000, 7)
	hot := 0
	const n = 100000
	for range n {
		if d.Next() < 10 {
			hot++
		}
	}
	// Under a uniform distribution the 10 lowest keys would get 0.1%.
	if hot < n/5 {
		t.Fatalf("the 10 hottest keys got %d of %d draws", hot, n)
	}
}

func TestQuantilesBoundTheSortedReference(t *testing.T) {
	rng := rand.New(rand.NewPCG(1, 2))
	var h Histogram
	var values []time.Duration
	for range 50000 {
		// Log-uniform over 1 us .. 100 ms.
		d := time.Duration(1000 * math.Pow(10, rng.Float64()*5))
		h.Record(d)
		values = append(values, d)
	}
	slices.Sort(values)
	for _, q := range []float64{0.5, 0.9, 0.99, 0.999, 1} {
		exact := values[int(q*float64(len(values)))-1]
		got := h.Quantile(q)
		if got < exact || float64(got) > float64(exact)*1.0625+1 {
			t.Fatalf("q=%v: got %v, exact %v", q, got, exact)
		}
	}
	if h.Max() != values[len(values)-1] || h.Count() != uint64(len(values)) {
		t.Fatalf("max %v count %d", h.Max(), h.Count())
	}
	var merged Histogram
	merged.Merge(&h)
	merged.Merge(&h)
	if merged.Count() != 2*h.Count() || merged.Quantile(0.5) != h.Quantile(0.5) {
		t.Fatal("merge changed the distribution")
	}
}

func TestReportsFormatAsTextAndJSON(t *testing.T) {
	r := OpResult{Op: "put", Duration: 2 * time.Second, Errors: map[string]uint64{"Unavailable": 3}}
	for range 100 {
		r.Hist.Record(time.Millisecond)
	}
	rep := r.Report()
	if rep.Ok != 100 || rep.PerSecond != 50 || rep.MaxUs != 1000 {
		t.Fatalf("report %+v", rep)
	}
	var text bytes.Buffer
	if err := WriteText(&text, []Report{rep}); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(text.String(), "Unavailable=3") || !strings.Contains(text.String(), "50 ops/s") {
		t.Fatalf("text %q", text.String())
	}
	var js bytes.Buffer
	if err := WriteJSON(&js, []Report{rep}); err != nil {
		t.Fatal(err)
	}
	var back []Report
	if err := json.Unmarshal(js.Bytes(), &back); err != nil || back[0].Ok != 100 {
		t.Fatalf("json %s: %v", js.String(), err)
	}
}
