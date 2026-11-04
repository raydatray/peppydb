// Package greet builds greetings. It exists to exercise the Go build until
// the gateway and placement driver arrive.
package greet

import (
	"context"
	"strings"

	"golang.org/x/sync/errgroup"
)

// Hello greets each name in order, one line per name.
func Hello(names []string) string {
	lines := make([]string, len(names))
	for i, name := range names {
		lines[i] = "hello, " + name
	}
	return strings.Join(lines, "\n")
}

// HelloAll calls greet for every name concurrently and returns the results in
// input order, or the first error. It uses errgroup, the fan-out pattern the
// gateway will use, so the build exercises a generated third-party rule.
func HelloAll(ctx context.Context, names []string, greet func(context.Context, string) (string, error)) ([]string, error) {
	out := make([]string, len(names))
	g, ctx := errgroup.WithContext(ctx)
	for i, name := range names {
		g.Go(func() error {
			s, err := greet(ctx, name)
			out[i] = s
			return err
		})
	}
	if err := g.Wait(); err != nil {
		return nil, err
	}
	return out, nil
}
