// Package load holds the pieces of peppy-load that do not need a server:
// key generation, the latency histogram, and result formatting
// (plan 0026, "The load generator").
package load

import (
	"encoding/binary"
	"fmt"
	"math/rand/v2"
	"strconv"
	"strings"
)

// Distribution chooses key indexes in [0, n).
type Distribution interface {
	Next() uint64
}

type uniform struct {
	rng *rand.Rand
	n   uint64
}

func (u *uniform) Next() uint64 { return u.rng.Uint64N(u.n) }

type zipf struct{ z *rand.Zipf }

func (z *zipf) Next() uint64 { return z.z.Uint64() }

// NewDistribution parses "uniform" or "zipf[:s]" (s > 1, default 1.1) and
// returns a generator over n keys, deterministic for a seed.
func NewDistribution(spec string, n uint64, seed uint64) (Distribution, error) {
	if n == 0 {
		return nil, fmt.Errorf("the key count must be positive")
	}
	rng := rand.New(rand.NewPCG(seed, seed^0x9e3779b97f4a7c15))
	name, arg, hasArg := strings.Cut(spec, ":")
	switch name {
	case "uniform":
		if hasArg {
			return nil, fmt.Errorf("uniform takes no parameter")
		}
		return &uniform{rng: rng, n: n}, nil
	case "zipf":
		s := 1.1
		if hasArg {
			var err error
			if s, err = strconv.ParseFloat(arg, 64); err != nil || s <= 1 {
				return nil, fmt.Errorf("zipf parameter must be a number above 1: %q", arg)
			}
		}
		return &zipf{z: rand.NewZipf(rng, s, 1, n-1)}, nil
	}
	return nil, fmt.Errorf("unknown distribution %q (uniform or zipf[:s])", spec)
}

// Key encodes index i as a key of exactly size bytes: the big-endian index
// in the last 8 bytes, padded in front with a fixed byte, so keys sort by
// index. size must be at least 1; below 8 the index is truncated.
func Key(i uint64, size int) []byte {
	key := make([]byte, size)
	var be [8]byte
	binary.BigEndian.PutUint64(be[:], i)
	if size >= 8 {
		for j := range size - 8 {
			key[j] = 'k'
		}
		copy(key[size-8:], be[:])
	} else {
		copy(key, be[8-size:])
	}
	return key
}

// Value returns a value of size bytes derived from i, so a reader can
// tell values apart.
func Value(i uint64, size int) []byte {
	value := make([]byte, size)
	for j := range value {
		value[j] = byte(i>>(8*(j%8))) ^ byte(j)
	}
	return value
}
