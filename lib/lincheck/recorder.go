package lincheck

import (
	"sync"
	"time"

	"github.com/anishathalye/porcupine"
)

// Stats summarizes the operations recorded during a run.
type Stats struct {
	Gets          int
	Puts          int
	Deletes       int
	UnknownWrites int
	FailedGets    int
}

// Recorder captures a thread-safe history of single-key KV operations with
// monotonic nanosecond timestamps relative to a shared start instant.
type Recorder struct {
	startedAt time.Time

	mu    sync.Mutex
	ops   []porcupine.Operation
	stats Stats
}

// NewRecorder creates an empty history recorder anchored at time.Now().
func NewRecorder() *Recorder {
	return &Recorder{startedAt: time.Now()}
}

// Now returns monotonic nanoseconds elapsed since the recorder was created.
func (r *Recorder) Now() int64 {
	return time.Since(r.startedAt).Nanoseconds()
}

// RecordGet appends a completed Get observation to the history.
func (r *Recorder) RecordGet(clientID int, key string, call, ret int64, found bool, val string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.ops = append(r.ops, porcupine.Operation{
		ClientId: clientID,
		Input:    Input{Kind: OpGet, Key: key},
		Call:     call,
		Output:   Output{Found: found, Value: val},
		Return:   ret,
	})
	r.stats.Gets++
}

// RecordFailedGet increments the failed-get counter without adding a history
// entry (a failed read reveals no state).
func (r *Recorder) RecordFailedGet() {
	r.mu.Lock()
	r.stats.FailedGets++
	r.mu.Unlock()
}

// RecordPut appends a completed or unknown-outcome Put to the history.
func (r *Recorder) RecordPut(clientID int, key, val string, call, ret int64, unknown bool) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.ops = append(r.ops, porcupine.Operation{
		ClientId: clientID,
		Input:    Input{Kind: OpPut, Key: key, Value: val},
		Call:     call,
		Output:   Output{Unknown: unknown},
		Return:   ret,
	})
	if unknown {
		r.stats.UnknownWrites++
	} else {
		r.stats.Puts++
	}
}

// RecordDelete appends a completed or unknown-outcome Delete to the history.
func (r *Recorder) RecordDelete(clientID int, key string, call, ret int64, unknown bool) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.ops = append(r.ops, porcupine.Operation{
		ClientId: clientID,
		Input:    Input{Kind: OpDelete, Key: key},
		Call:     call,
		Output:   Output{Unknown: unknown},
		Return:   ret,
	})
	if unknown {
		r.stats.UnknownWrites++
	} else {
		r.stats.Deletes++
	}
}

// Operations returns a snapshot copy of all recorded operations.
func (r *Recorder) Operations() []porcupine.Operation {
	r.mu.Lock()
	defer r.mu.Unlock()
	cp := make([]porcupine.Operation, len(r.ops))
	copy(cp, r.ops)
	return cp
}

// Stats returns a copy of the operation counts.
func (r *Recorder) Stats() Stats {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.stats
}
