// Package lincheck provides a Porcupine-based linearizability checker and
// concurrent workload harness for peppydb single-key KV operations (plan 0033).
//
// Operations modeled: Get, Put, and Delete partitioned by key. Range scans are
// intentionally excluded because kv.proto guarantees linearizability per key,
// not across storage leaves or scan pages.
package lincheck

import (
	"fmt"
	"sort"
	"time"

	"github.com/anishathalye/porcupine"
)

// OpKind identifies a single-key KV operation in a recorded history.
type OpKind int

const (
	OpGet OpKind = iota
	OpPut
	OpDelete
)

// Input is the Porcupine operation input for one single-key RPC.
type Input struct {
	Kind  OpKind
	Key   string
	Value string
}

// Output is the Porcupine operation output for one single-key RPC.
type Output struct {
	Found   bool
	Value   string
	Unknown bool
}

type kvState struct {
	present bool
	value   string
}

// KVModel returns the key-partitioned Porcupine model for single-key Get, Put,
// and Delete operations.
func KVModel() porcupine.Model {
	return porcupine.Model{
		Partition: partitionByKey,
		Init: func() any {
			return kvState{}
		},
		Step: func(state, input, output any) (bool, any) {
			st := state.(kvState)
			in := input.(Input)
			out := output.(Output)
			switch in.Kind {
			case OpGet:
				if out.Unknown {
					return true, st
				}
				ok := st.present == out.Found && (!st.present || st.value == out.Value)
				return ok, st
			case OpPut:
				return true, kvState{present: true, value: in.Value}
			case OpDelete:
				return true, kvState{present: false}
			default:
				return false, st
			}
		},
		Equal: func(s1, s2 any) bool {
			return s1.(kvState) == s2.(kvState)
		},
		DescribeOperation: func(input, output any) string {
			in := input.(Input)
			out := output.(Output)
			switch in.Kind {
			case OpGet:
				if out.Unknown {
					return fmt.Sprintf("Get(%q) -> unknown", in.Key)
				}
				if !out.Found {
					return fmt.Sprintf("Get(%q) -> absent", in.Key)
				}
				return fmt.Sprintf("Get(%q) -> %q", in.Key, out.Value)
			case OpPut:
				if out.Unknown {
					return fmt.Sprintf("Put(%q, %q) -> unknown", in.Key, in.Value)
				}
				return fmt.Sprintf("Put(%q, %q) -> ok", in.Key, in.Value)
			case OpDelete:
				if out.Unknown {
					return fmt.Sprintf("Delete(%q) -> unknown", in.Key)
				}
				return fmt.Sprintf("Delete(%q) -> ok", in.Key)
			default:
				return "unknown op"
			}
		},
		DescribeState: func(state any) string {
			st := state.(kvState)
			if !st.present {
				return "absent"
			}
			return fmt.Sprintf("%q", st.value)
		},
	}
}

func partitionByKey(history []porcupine.Operation) [][]porcupine.Operation {
	byKey := make(map[string][]porcupine.Operation)
	keys := make([]string, 0)
	for _, op := range history {
		key := op.Input.(Input).Key
		if _, exists := byKey[key]; !exists {
			keys = append(keys, key)
		}
		byKey[key] = append(byKey[key], op)
	}
	sort.Strings(keys)
	partitions := make([][]porcupine.Operation, 0, len(keys))
	for _, k := range keys {
		partitions = append(partitions, byKey[k])
	}
	return partitions
}

// NormalizeUnknowns returns a copy of ops in which every unknown-outcome write
// has Return set strictly past the last completed timestamp in the history.
// Porcupine can therefore linearize an applied unknown write anywhere after
// Call, or place an unapplied unknown write after all completed observations.
func NormalizeUnknowns(ops []porcupine.Operation) []porcupine.Operation {
	var maxTime int64
	for _, op := range ops {
		if op.Call > maxTime {
			maxTime = op.Call
		}
		if out, ok := op.Output.(Output); ok && !out.Unknown {
			if op.Return > maxTime {
				maxTime = op.Return
			}
		}
	}
	endOfHistory := maxTime + 1
	out := make([]porcupine.Operation, len(ops))
	for i, op := range ops {
		if res, ok := op.Output.(Output); ok && res.Unknown {
			op.Return = endOfHistory
		}
		out[i] = op
	}
	return out
}

// Check normalizes unknown-outcome intervals and runs Porcupine with timeout.
func Check(ops []porcupine.Operation, timeout time.Duration) (porcupine.CheckResult, porcupine.LinearizationInfo) {
	return porcupine.CheckOperationsVerbose(KVModel(), NormalizeUnknowns(ops), timeout)
}

// WriteVisualization renders Porcupine's HTML timeline for debugging failures.
func WriteVisualization(path string, info porcupine.LinearizationInfo) error {
	return porcupine.VisualizePath(KVModel(), info, path)
}
