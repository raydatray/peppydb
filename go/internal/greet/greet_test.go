package greet

import "testing"

func TestHello(t *testing.T) {
	got := Hello([]string{"buck2", "go"})
	want := "hello, buck2\nhello, go"
	if got != want {
		t.Fatalf("Hello() = %q, want %q", got, want)
	}
}
