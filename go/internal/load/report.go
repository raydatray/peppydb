package load

import (
	"encoding/json"
	"fmt"
	"io"
	"sort"
	"time"
)

// OpResult is the outcome of one operation type in a run.
type OpResult struct {
	Op       string
	Hist     Histogram
	Errors   map[string]uint64 // by gRPC status code name
	Duration time.Duration
}

// Report is the machine-readable form of an OpResult.
type Report struct {
	Op        string            `json:"op"`
	Ok        uint64            `json:"ok"`
	Errors    map[string]uint64 `json:"errors"`
	PerSecond float64           `json:"ops_per_second"`
	P50Us     float64           `json:"p50_us"`
	P99Us     float64           `json:"p99_us"`
	P999Us    float64           `json:"p999_us"`
	MaxUs     float64           `json:"max_us"`
}

func micros(d time.Duration) float64 { return float64(d) / float64(time.Microsecond) }

// Report summarizes r. Throughput counts successful operations only.
func (r *OpResult) Report() Report {
	errors := r.Errors
	if errors == nil {
		errors = map[string]uint64{}
	}
	perSecond := 0.0
	if r.Duration > 0 {
		perSecond = float64(r.Hist.Count()) / r.Duration.Seconds()
	}
	return Report{
		Op:        r.Op,
		Ok:        r.Hist.Count(),
		Errors:    errors,
		PerSecond: perSecond,
		P50Us:     micros(r.Hist.Quantile(0.50)),
		P99Us:     micros(r.Hist.Quantile(0.99)),
		P999Us:    micros(r.Hist.Quantile(0.999)),
		MaxUs:     micros(r.Hist.Max()),
	}
}

// WriteText prints one line per operation type.
func WriteText(w io.Writer, reports []Report) error {
	for _, r := range reports {
		codes := make([]string, 0, len(r.Errors))
		for code := range r.Errors {
			codes = append(codes, code)
		}
		sort.Strings(codes)
		errs := ""
		for _, code := range codes {
			errs += fmt.Sprintf(" %s=%d", code, r.Errors[code])
		}
		if errs == "" {
			errs = " none"
		}
		_, err := fmt.Fprintf(w,
			"%-4s %10.0f ops/s  ok=%d  p50=%.0fus p99=%.0fus p99.9=%.0fus max=%.0fus  errors:%s\n",
			r.Op, r.PerSecond, r.Ok, r.P50Us, r.P99Us, r.P999Us, r.MaxUs, errs)
		if err != nil {
			return err
		}
	}
	return nil
}

// WriteJSON prints the reports as one JSON array.
func WriteJSON(w io.Writer, reports []Report) error {
	enc := json.NewEncoder(w)
	enc.SetIndent("", "  ")
	return enc.Encode(reports)
}
