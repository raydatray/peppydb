package main

import (
	"testing"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	kvv1 "peppydb/proto/kv/v1/go"
)

func TestParseAddrs(t *testing.T) {
	got, err := parseAddrs("127.0.0.1:7420, 127.0.0.1:7421 ,127.0.0.1:7422")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if len(got) != 3 || got[1] != "127.0.0.1:7421" {
		t.Fatalf("got %v", got)
	}
	if _, err := parseAddrs(" , "); err == nil {
		t.Fatalf("expected error for empty addrs")
	}
}

func TestExtractLeaderHint(t *testing.T) {
	st, err := status.New(codes.Unavailable, "not leader").WithDetails(&kvv1.LeaderHint{
		LeaderId:   2,
		LeaderAddr: "10.0.0.2:7420",
	})
	if err != nil {
		t.Fatalf("WithDetails: %v", err)
	}
	if got := extractLeaderHint(st.Err()); got != "10.0.0.2:7420" {
		t.Fatalf("got %q, want %q", got, "10.0.0.2:7420")
	}
	if got := extractLeaderHint(status.Error(codes.Unavailable, "no leader")); got != "" {
		t.Fatalf("expected empty hint, got %q", got)
	}
}
