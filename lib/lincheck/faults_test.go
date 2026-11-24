package lincheck

import (
	"context"
	"fmt"
	"testing"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"

	kvv1 "peppydb/proto/kv/v1/go"
)

func TestE2ESeededMultiFaultAndMembershipLinearizable(t *testing.T) {
	serverBin, gwBin := requireBinaries(t)

	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()

	cluster, err := StartCluster(ctx, ClusterConfig{
		ServerBin:         serverBin,
		GatewayBin:        gwBin,
		Nodes:             3,
		BaseDir:           t.TempDir(),
		SnapshotAfter:     30,
		KeepBelowSnapshot: 5,
	})
	if err != nil {
		t.Fatalf("StartCluster: %v", err)
	}
	t.Cleanup(cluster.Close)

	if _, _, err := cluster.WaitLeader(ctx, 0); err != nil {
		t.Fatalf("WaitLeader: %v", err)
	}

	gw, err := cluster.StartGateway(ctx, cluster.Addrs(), false)
	if err != nil {
		t.Fatalf("StartGateway: %v", err)
	}
	gwConn, err := grpc.NewClient(gw.Addr(), grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		t.Fatalf("dial gateway: %v", err)
	}
	t.Cleanup(func() { _ = gwConn.Close() })
	kv := kvv1.NewKvServiceClient(gwConn)

	const numKeys = 10
	const seed = 20261124
	rec, faultErr := RunWorkload(ctx, kv, WorkloadConfig{
		Workers:       8,
		Keys:          numKeys,
		Duration:      6 * time.Second,
		ReadPercent:   45,
		DeletePercent: 15,
		OpTimeout:     2 * time.Second,
		ErrorBackoff:  50 * time.Millisecond,
		Seed:          seed,
		Fault: func(runCtx context.Context) error {
			return RunFaultSchedule(runCtx, cluster, FaultConfig{
				Seed:             seed,
				Rounds:           3,
				EnableMembership: true,
				Logf:             t.Logf,
			})
		},
	})
	if faultErr != nil {
		t.Fatalf("fault schedule failed (seed=%d): %v", seed, faultErr)
	}

	for k := range numKeys {
		key := fmt.Sprintf("k%d", k)
		call := rec.Now()
		resp, err := kv.Get(ctx, &kvv1.GetRequest{Key: []byte(key)})
		ret := rec.Now()
		if err != nil {
			t.Fatalf("final Get(%s): %v\ngateway stderr:\n%s", key, err, gw.Stderr())
		}
		rec.RecordGet(99, key, call, ret, resp.Value != nil, string(resp.Value))
	}

	st := rec.Stats()
	t.Logf("seeded multi-fault stats (seed=%d): %+v", seed, st)
	if st.Gets < 50 || st.Puts < 50 {
		t.Fatalf("too few completed ops: %+v", st)
	}
	assertLinearizable(t, "seeded_multi_fault_membership", rec)
}
