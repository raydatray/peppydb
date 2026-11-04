package greet

import (
	"context"
	"errors"
	"testing"
)

func TestHello(t *testing.T) {
	got := Hello([]string{"buck2", "go"})
	want := "hello, buck2\nhello, go"
	if got != want {
		t.Fatalf("Hello() = %q, want %q", got, want)
	}
}

func TestHelloAllKeepsOrder(t *testing.T) {
	greet := func(_ context.Context, name string) (string, error) { return "hi " + name, nil }
	got, err := HelloAll(context.Background(), []string{"a", "b", "c"}, greet)
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 3 || got[0] != "hi a" || got[1] != "hi b" || got[2] != "hi c" {
		t.Fatalf("HelloAll() = %q", got)
	}
}

func TestHelloAllReturnsAnError(t *testing.T) {
	boom := errors.New("boom")
	greet := func(_ context.Context, name string) (string, error) {
		if name == "b" {
			return "", boom
		}
		return name, nil
	}
	if _, err := HelloAll(context.Background(), []string{"a", "b"}, greet); !errors.Is(err, boom) {
		t.Fatalf("HelloAll() error = %v, want %v", err, boom)
	}
}
