// Command hello prints a greeting. It checks that Go builds under Buck2.
package main

import (
	"fmt"
	"os"

	"peppydb/go/internal/greet"
)

func main() {
	names := os.Args[1:]
	if len(names) == 0 {
		names = []string{"peppydb"}
	}
	fmt.Println(greet.Hello(names))
}
