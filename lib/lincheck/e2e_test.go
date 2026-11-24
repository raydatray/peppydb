package lincheck

import (
	"context"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/anishathalye/porcupine"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/health/grpc_health_v1"
	"google.golang.org/grpc/status"

	kvv1 "peppydb/proto/kv/v1/go"
)

func requireBinaries(t *testing.T) (string, string) {
	t.Helper()
	serverBin, ok := FindBinary("PEPPYDB_BIN")
	if !ok {
		t.Skip("PEPPYDB_BIN not set; run via `tools/buck2 test //lib/lincheck/...`")
	}
	gwBin, ok := FindBinary("PEPPYDB_GATEWAY_BIN")
	if !ok {
		t.Skip("PEPPYDB_GATEWAY_BIN not set; run via `tools/buck2 test //lib/lincheck/...`")
	}
	return serverBin, gwBin
}

func assertLinearizable(t *testing.T, name string, rec *Recorder) {
	t.Helper()
	ops := rec.Operations()
	res, info := Check(ops, 10*time.Second)
	if res == porcupine.Ok {
		return
	}
	visDir := os.Getenv("LINCHECK_VIS_DIR")
	if visDir == "" {
		visDir = t.TempDir()
	}
	visPath := filepath.Join(visDir, name+".html")
	_ = WriteVisualization(visPath, info)
	t.Fatalf("linearizability check failed: result=%v stats=%+v ops=%d visualization=%s",
		res, rec.Stats(), len(ops), visPath)
}

func TestE2ESingleNodeGatewayCRUDScanHealthMetrics(t *testing.T) {
	serverBin, gwBin := requireBinaries(t)

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	cluster, err := StartCluster(ctx, ClusterConfig{
		ServerBin:  serverBin,
		GatewayBin: gwBin,
		Nodes:      1,
		BaseDir:    t.TempDir(),
	})
	if err != nil {
		t.Fatalf("StartCluster: %v", err)
	}
	t.Cleanup(cluster.Close)

	if _, _, err := cluster.WaitLeader(ctx, 0); err != nil {
		t.Fatalf("WaitLeader: %v\nstderr:\n%s", err, cluster.NodeStderr(1))
	}

	gw, err := cluster.StartGateway(ctx, cluster.Addrs(), true)
	if err != nil {
		t.Fatalf("StartGateway: %v", err)
	}

	conn, err := grpc.NewClient(gw.Addr(), grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		t.Fatalf("dial gateway: %v", err)
	}
	t.Cleanup(func() { _ = conn.Close() })

	// Health check.
	hc := grpc_health_v1.NewHealthClient(conn)
	hResp, err := hc.Check(ctx, &grpc_health_v1.HealthCheckRequest{Service: "peppydb.kv.v1.KvService"})
	if err != nil || hResp.Status != grpc_health_v1.HealthCheckResponse_SERVING {
		t.Fatalf("health = (%v, %v), want SERVING", hResp, err)
	}

	kv := kvv1.NewKvServiceClient(conn)

	// Absent vs empty value vs non-empty value.
	gResp, err := kv.Get(ctx, &kvv1.GetRequest{Key: []byte("missing")})
	if err != nil || gResp.Value != nil {
		t.Fatalf("Get missing = (%v, %v), want (nil, nil)", gResp, err)
	}
	if _, err := kv.Put(ctx, &kvv1.PutRequest{Key: []byte("k0"), Value: []byte{}}); err != nil {
		t.Fatalf("Put empty value: %v", err)
	}
	gResp, err = kv.Get(ctx, &kvv1.GetRequest{Key: []byte("k0")})
	if err != nil || gResp.Value == nil || len(gResp.Value) != 0 {
		t.Fatalf("Get empty value = (%v, %v), want (empty non-nil, nil)", gResp, err)
	}

	for i := 1; i <= 5; i++ {
		k := fmt.Sprintf("k%d", i)
		v := fmt.Sprintf("v%d", i)
		if _, err := kv.Put(ctx, &kvv1.PutRequest{Key: []byte(k), Value: []byte(v)}); err != nil {
			t.Fatalf("Put %s: %v", k, err)
		}
	}

	// Scan across multiple pages.
	var scanned []string
	var start []byte
	for {
		sResp, err := kv.Scan(ctx, &kvv1.ScanRequest{
			Start:    start,
			PageSize: 2,
		})
		if err != nil {
			t.Fatalf("Scan: %v", err)
		}
		for _, rec := range sResp.Records {
			scanned = append(scanned, string(rec.Key)+"="+string(rec.Value))
		}
		if sResp.NextStart == nil {
			break
		}
		start = sResp.NextStart
	}
	wantScan := "k0=,k1=v1,k2=v2,k3=v3,k4=v4,k5=v5"
	if got := strings.Join(scanned, ","); got != wantScan {
		t.Fatalf("scanned = %q, want %q", got, wantScan)
	}

	// Delete + InvalidArgument check.
	if _, err := kv.Delete(ctx, &kvv1.DeleteRequest{Key: []byte("k3")}); err != nil {
		t.Fatalf("Delete k3: %v", err)
	}
	gResp, err = kv.Get(ctx, &kvv1.GetRequest{Key: []byte("k3")})
	if err != nil || gResp.Value != nil {
		t.Fatalf("Get deleted k3 = (%v, %v), want (nil, nil)", gResp, err)
	}
	if _, err := kv.Get(ctx, &kvv1.GetRequest{Key: nil}); status.Code(err) != codes.InvalidArgument {
		t.Fatalf("empty key err = %v, want InvalidArgument", err)
	}

	// Metrics endpoint check.
	resp, err := http.Get("http://" + gw.MetricsAddr() + "/metrics")
	if err != nil {
		t.Fatalf("GET /metrics: %v", err)
	}
	body, _ := io.ReadAll(resp.Body)
	_ = resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("GET /metrics status = %d, want 200", resp.StatusCode)
	}
	text := string(body)
	for _, want := range []string{"peppydb_gateway_rpc_requests_total", "go_goroutines"} {
		if !strings.Contains(text, want) {
			t.Errorf("/metrics missing %q", want)
		}
	}
}

func TestE2EThreeNodeGatewayLeaderFailoverLinearizable(t *testing.T) {
	serverBin, gwBin := requireBinaries(t)

	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()

	cluster, err := StartCluster(ctx, ClusterConfig{
		ServerBin:         serverBin,
		GatewayBin:        gwBin,
		Nodes:             3,
		BaseDir:           t.TempDir(),
		SnapshotAfter:     200,
		KeepBelowSnapshot: 20,
	})
	if err != nil {
		t.Fatalf("StartCluster: %v", err)
	}
	t.Cleanup(cluster.Close)

	leader1, leader1Addr, err := cluster.WaitLeader(ctx, 0)
	if err != nil {
		t.Fatalf("WaitLeader: %v", err)
	}

	// Pick a follower and wait until it knows leader1, then verify real
	// Rust->Go google.rpc.Status LeaderHint interop across the wire.
	followerID := uint64(1)
	if followerID == leader1 {
		followerID = 2
	}
	if err := cluster.WaitFollowerKnowsLeader(ctx, followerID, leader1); err != nil {
		t.Fatalf("WaitFollowerKnowsLeader: %v", err)
	}
	fConn, err := grpc.NewClient(cluster.Addr(followerID), grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		t.Fatalf("dial follower: %v", err)
	}
	_, fErr := kvv1.NewKvServiceClient(fConn).Put(ctx, &kvv1.PutRequest{
		Key:   []byte("hint-check"),
		Value: []byte("v"),
	})
	_ = fConn.Close()
	if status.Code(fErr) != codes.Unavailable {
		t.Fatalf("follower Put err = %v, want Unavailable", fErr)
	}
	var gotHint *kvv1.LeaderHint
	if st, ok := status.FromError(fErr); ok {
		for _, d := range st.Details() {
			if h, ok := d.(*kvv1.LeaderHint); ok {
				gotHint = h
			}
		}
	}
	if gotHint == nil || gotHint.LeaderId != leader1 || gotHint.LeaderAddr != leader1Addr {
		t.Fatalf("follower LeaderHint = %+v, want {LeaderId:%d LeaderAddr:%q}", gotHint, leader1, leader1Addr)
	}

	// Start gateway seeded with followers first so it discovers/redirects to leader1.
	backends := []string{cluster.Addr(followerID)}
	for id := uint64(1); id <= 3; id++ {
		if id != followerID {
			backends = append(backends, cluster.Addr(id))
		}
	}
	gw, err := cluster.StartGateway(ctx, backends, false)
	if err != nil {
		t.Fatalf("StartGateway: %v", err)
	}

	gwConn, err := grpc.NewClient(gw.Addr(), grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		t.Fatalf("dial gateway: %v", err)
	}
	t.Cleanup(func() { _ = gwConn.Close() })
	kv := kvv1.NewKvServiceClient(gwConn)

	// Run concurrent workload while killing leader1 with SIGKILL, failing over
	// to a surviving node, and restarting leader1.
	const numKeys = 5
	rec, faultErr := RunWorkload(ctx, kv, WorkloadConfig{
		Workers:       8,
		Keys:          numKeys,
		Duration:      3500 * time.Millisecond,
		ReadPercent:   45,
		DeletePercent: 15,
		OpTimeout:     2 * time.Second,
		Seed:          42,
		Fault: func(runCtx context.Context) error {
			select {
			case <-time.After(600 * time.Millisecond):
			case <-runCtx.Done():
				return nil
			}
			if err := cluster.Kill(leader1); err != nil {
				return fmt.Errorf("kill leader %d: %w", leader1, err)
			}
			waitCtx, waitCancel := context.WithTimeout(context.Background(), 15*time.Second)
			defer waitCancel()
			leader2, _, err := cluster.WaitLeader(waitCtx, leader1)
			if err != nil {
				return fmt.Errorf("failover after killing leader %d: %w", leader1, err)
			}
			t.Logf("leader failover: %d -> %d", leader1, leader2)
			select {
			case <-time.After(800 * time.Millisecond):
			case <-runCtx.Done():
			}
			if err := cluster.Restart(waitCtx, leader1); err != nil {
				return fmt.Errorf("restart old leader %d: %w", leader1, err)
			}
			return nil
		},
	})
	if faultErr != nil {
		t.Fatalf("fault injection error: %v", faultErr)
	}

	// Final reads across every key after failover so all committed writes are observed.
	for k := range numKeys {
		key := fmt.Sprintf("k%d", k)
		call := rec.Now()
		resp, err := kv.Get(ctx, &kvv1.GetRequest{Key: []byte(key)})
		ret := rec.Now()
		if err != nil {
			t.Fatalf("final Get(%s) after failover: %v\ngateway stderr:\n%s", key, err, gw.Stderr())
		}
		rec.RecordGet(99, key, call, ret, resp.Value != nil, string(resp.Value))
	}

	st := rec.Stats()
	t.Logf("3-node failover stats: %+v", st)
	if st.Gets < 20 || st.Puts < 20 {
		t.Fatalf("too few completed ops during failover run: %+v", st)
	}
	assertLinearizable(t, "three_node_failover", rec)
}
