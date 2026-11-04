// Package greet builds greetings. It exists to exercise the Go build until
// the gateway and placement driver arrive.
package greet

import "strings"

// Hello greets each name in order, one line per name.
func Hello(names []string) string {
	lines := make([]string, len(names))
	for i, name := range names {
		lines[i] = "hello, " + name
	}
	return strings.Join(lines, "\n")
}
