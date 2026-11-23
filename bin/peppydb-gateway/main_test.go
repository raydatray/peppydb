package main

import "testing"

func TestParseBackends(t *testing.T) {
	got, err := parseBackends("127.0.0.1:7420, 127.0.0.1:7421 ,127.0.0.1:7422")
	if err != nil {
		t.Fatalf("parseBackends: %v", err)
	}
	if len(got) != 3 || got[0] != "127.0.0.1:7420" || got[2] != "127.0.0.1:7422" {
		t.Fatalf("got %v", got)
	}
	if _, err := parseBackends(" , "); err == nil {
		t.Fatal("expected error for empty backends")
	}
}
