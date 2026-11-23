package main

import (
	"testing"

	"google.golang.org/grpc/metadata"
	"google.golang.org/protobuf/proto"

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
	raw, err := proto.Marshal(&kvv1.LeaderHint{
		LeaderId:   2,
		LeaderAddr: "10.0.0.2:7420",
	})
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	md := metadata.Pairs("grpc-status-details-bin", string(raw))
	if got := extractLeaderHint(md); got != "10.0.0.2:7420" {
		t.Fatalf("got %q, want %q", got, "10.0.0.2:7420")
	}
	if got := extractLeaderHint(metadata.New(nil)); got != "" {
		t.Fatalf("expected empty hint, got %q", got)
	}
}
