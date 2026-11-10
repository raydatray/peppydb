package load

import (
	"math/bits"
	"time"
)

// subBuckets per power of two: values are recorded with a relative error
// below 1/subBuckets (6.25%).
const subBucketBits = 4
const subBuckets = 1 << subBucketBits

// Histogram records latencies in a log-linear layout: each power of two of
// nanoseconds is split into 16 equal buckets. It is not safe for
// concurrent use; give each worker its own and Merge them.
type Histogram struct {
	counts [64 * subBuckets]uint64
	total  uint64
	max    time.Duration
}

func bucket(ns uint64) int {
	if ns < subBuckets {
		return int(ns)
	}
	exp := bits.Len64(ns) - 1 // ns in [2^exp, 2^(exp+1))
	sub := (ns >> (exp - subBucketBits)) & (subBuckets - 1)
	return (exp-subBucketBits+1)*subBuckets + int(sub)
}

// upper returns the largest value that falls into bucket b.
func upper(b int) uint64 {
	if b < subBuckets {
		return uint64(b)
	}
	exp := b/subBuckets + subBucketBits - 1
	sub := uint64(b % subBuckets)
	low := (1 << exp) | sub<<(exp-subBucketBits)
	return low + (1 << (exp - subBucketBits)) - 1
}

// Record adds one latency.
func (h *Histogram) Record(d time.Duration) {
	if d < 0 {
		d = 0
	}
	h.counts[bucket(uint64(d))]++
	h.total++
	h.max = max(h.max, d)
}

// Merge adds every value recorded in o.
func (h *Histogram) Merge(o *Histogram) {
	for i, c := range o.counts {
		h.counts[i] += c
	}
	h.total += o.total
	h.max = max(h.max, o.max)
}

// Count is the number of recorded values.
func (h *Histogram) Count() uint64 { return h.total }

// Max is the largest recorded value, exactly.
func (h *Histogram) Max() time.Duration { return h.max }

// Quantile returns an upper bound for the q-quantile (0 < q <= 1): the top
// of the bucket that holds it, capped at Max. Zero if nothing is recorded.
func (h *Histogram) Quantile(q float64) time.Duration {
	if h.total == 0 {
		return 0
	}
	rank := uint64(q * float64(h.total))
	if rank < 1 {
		rank = 1
	}
	var seen uint64
	for b, c := range h.counts {
		seen += c
		if seen >= rank {
			return min(time.Duration(upper(b)), h.max)
		}
	}
	return h.max
}
