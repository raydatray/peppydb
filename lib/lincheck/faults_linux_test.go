//go:build linux

package lincheck

import (
	"context"
	"fmt"
	"os"
	"testing"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"

	kvv1 "peppydb/proto/kv/v1/go"
)

// TestE2ELinuxIPTablesPartitionFaultsLinearizable runs seeded iptables network
// partitions alongside leader/follower crashes through peppydb-gateway and
// checks client-visible linearizability with Porcupine.
//
// Run on Linux (for example WSL2) with CAP_NET_ADMIN:
//
//	LINCHECK_IPTABLES=1 tools/buck2 run //lib/lincheck:lincheck-test -- -test.v -test.run TestE2ELinuxIPTablesPartitionFaultsLinearizable
func TestE2ELinuxIPTablesPartitionFaultsLinearizable(t *testing.T) {
	if os.Getenv("LINCHECK_IPTABLES") != "1" {
		t.Skip("set LINCHECK_IPTABLES=1 on Linux (with CAP_NET_ADMIN) to run iptables partition fault tests")
	}
	serverBin, gwBin := requireBinaries(t)

	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()

	cluster, err := StartCluster(ctx, ClusterConfig{
		ServerBin:         serverBin,
		GatewayBin:        gwBin,
		Nodes:             3,
		BaseDir:           t.TempDir(),
		SnapshotAfter:     40,
		KeepBelowSnapshot: 5,
		BindIPs:           []string{"127.0.0.21", "127.0.0.22", "127.0.0.23"},
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
	const seed = 20261125
	rec, faultErr := RunWorkload(ctx, kv, WorkloadConfig{
		Workers:       8,
		Keys:          numKeys,
		Duration:      8 * time.Second,
		ReadPercent:   45,
		DeletePercent: 15,
		OpTimeout:     2 * time.Second,
		ErrorBackoff:  50 * time.Millisecond,
		Seed:          seed,
		Fault: func(runCtx context.Context) error {
			return RunFaultSchedule(runCtx, cluster, FaultConfig{
				Seed:       seed,
				Rounds:     3,
				ExtraFault: IPTablesPartitionFault,
				Logf:       t.Logf,
			})
		},
	})
	if faultErr != nil {
		t.Fatalf("iptables fault schedule failed (seed=%d): %v", seed, faultErr)
	}

	for k := range numKeys {
		key := fmt.Sprintf("k%d", k)
		call := rec.Now()
		resp, err := kv.Get(ctx, &kvv1.GetRequest{Key: []byte(key)})
		ret := rec.Now()
		if err != nil {
			t.Fatalf("final Get(%s): %v", key, err)
		}
		rec.RecordGet(99, key, call, ret, resp.Value != nil, string(resp.Value))
	}

	st := rec.Stats()
	t.Logf("linux iptables fault stats (seed=%d): %+v", seed, st)
	if st.Gets < 50 || st.Puts < 50 {
		t.Fatalf("too few completed ops: %+v", st)
	}
	assertLinearizable(t, "linux_iptables_faults", rec)
}
