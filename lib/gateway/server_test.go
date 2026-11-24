package gateway

import (
	"context"
	"io"
	"net"
	"net/http"
	"sort"
	"strings"
	"sync"
	"testing"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/health/grpc_health_v1"
	"google.golang.org/grpc/status"

	kvv1 "peppydb/proto/kv/v1/go"
)

type fakeBackendServer struct {
	kvv1.UnimplementedKvServiceServer
	kvv1.UnimplementedAdminServiceServer

	addr string
	srv  *grpc.Server

	mu         sync.Mutex
	role       string
	leaderID   uint64
	leaderAddr string
	data       map[string][]byte
}

func startFakeBackend(t *testing.T, role string) *fakeBackendServer {
	t.Helper()
	lis, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	b := &fakeBackendServer{
		addr: lis.Addr().String(),
		srv:  grpc.NewServer(),
		role: role,
		data: make(map[string][]byte),
	}
	if role == "leader" {
		b.leaderID = 1
		b.leaderAddr = b.addr
	}
	kvv1.RegisterKvServiceServer(b.srv, b)
	kvv1.RegisterAdminServiceServer(b.srv, b)
	go func() {
		_ = b.srv.Serve(lis)
	}()
	t.Cleanup(b.srv.Stop)
	return b
}

func (b *fakeBackendServer) notLeader() error {
	st, err := status.Newf(codes.Unavailable, "not leader; leader is %s", b.leaderAddr).
		WithDetails(&kvv1.LeaderHint{
			LeaderId:   b.leaderID,
			LeaderAddr: b.leaderAddr,
		})
	if err != nil {
		return status.Error(codes.Unavailable, "not leader")
	}
	return st.Err()
}

func (b *fakeBackendServer) Status(_ context.Context, _ *kvv1.StatusRequest) (*kvv1.StatusResponse, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	id := b.leaderID
	addr := b.leaderAddr
	return &kvv1.StatusResponse{
		Role:       b.role,
		LeaderId:   &id,
		LeaderAddr: &addr,
	}, nil
}

func (b *fakeBackendServer) Get(_ context.Context, req *kvv1.GetRequest) (*kvv1.GetResponse, error) {
	if len(req.Key) == 0 {
		return nil, status.Error(codes.InvalidArgument, "empty key")
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.role != "leader" {
		return nil, b.notLeader()
	}
	val, ok := b.data[string(req.Key)]
	if !ok {
		return &kvv1.GetResponse{}, nil
	}
	return &kvv1.GetResponse{Value: append([]byte(nil), val...)}, nil
}

func (b *fakeBackendServer) Put(_ context.Context, req *kvv1.PutRequest) (*kvv1.PutResponse, error) {
	if len(req.Key) == 0 {
		return nil, status.Error(codes.InvalidArgument, "empty key")
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.role != "leader" {
		return nil, b.notLeader()
	}
	b.data[string(req.Key)] = append([]byte(nil), req.Value...)
	return &kvv1.PutResponse{}, nil
}

func (b *fakeBackendServer) Delete(_ context.Context, req *kvv1.DeleteRequest) (*kvv1.DeleteResponse, error) {
	if len(req.Key) == 0 {
		return nil, status.Error(codes.InvalidArgument, "empty key")
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.role != "leader" {
		return nil, b.notLeader()
	}
	delete(b.data, string(req.Key))
	return &kvv1.DeleteResponse{}, nil
}

func (b *fakeBackendServer) Scan(_ context.Context, req *kvv1.ScanRequest) (*kvv1.ScanResponse, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.role != "leader" {
		return nil, b.notLeader()
	}
	keys := make([]string, 0, len(b.data))
	for k := range b.data {
		if req.Start != nil && k < string(req.Start) {
			continue
		}
		if req.End != nil && k >= string(req.End) {
			continue
		}
		keys = append(keys, k)
	}
	sort.Strings(keys)
	limit := int(req.PageSize)
	if limit <= 0 || limit > len(keys) {
		limit = len(keys)
	}
	resp := &kvv1.ScanResponse{}
	for _, k := range keys[:limit] {
		resp.Records = append(resp.Records, &kvv1.KeyValue{
			Key:   []byte(k),
			Value: append([]byte(nil), b.data[k]...),
		})
	}
	if limit < len(keys) {
		resp.NextStart = []byte(keys[limit])
	}
	return resp, nil
}

func TestGatewayCRUDScanHealthAndMetrics(t *testing.T) {
	leader := startFakeBackend(t, "leader")
	follower := startFakeBackend(t, "follower")
	follower.mu.Lock()
	follower.leaderID = 1
	follower.leaderAddr = leader.addr
	follower.mu.Unlock()

	metrics := NewMetrics()
	router, err := NewStaticRouter([]string{follower.addr, leader.addr}, Config{
		RequestTimeout: 5 * time.Second,
	}, metrics)
	if err != nil {
		t.Fatalf("NewStaticRouter: %v", err)
	}

	grpcLis, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen grpc: %v", err)
	}
	metricsLis, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen metrics: %v", err)
	}

	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() {
		done <- Serve(ctx, grpcLis, metricsLis, router, Config{RequestTimeout: 5 * time.Second}, metrics)
	}()

	conn, err := grpc.NewClient(grpcLis.Addr().String(), grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		t.Fatalf("dial gateway: %v", err)
	}
	t.Cleanup(func() { _ = conn.Close() })

	// Health check reports SERVING.
	hc := grpc_health_v1.NewHealthClient(conn)
	hResp, err := hc.Check(context.Background(), &grpc_health_v1.HealthCheckRequest{Service: kvServiceName})
	if err != nil {
		t.Fatalf("health check: %v", err)
	}
	if hResp.Status != grpc_health_v1.HealthCheckResponse_SERVING {
		t.Fatalf("health status = %v, want SERVING", hResp.Status)
	}

	kv := kvv1.NewKvServiceClient(conn)

	// Put, Get, Delete, and empty-key InvalidArgument propagation.
	for _, pair := range [][2]string{{"a", "1"}, {"b", "2"}, {"c", "3"}} {
		if _, err := kv.Put(context.Background(), &kvv1.PutRequest{
			Key:   []byte(pair[0]),
			Value: []byte(pair[1]),
		}); err != nil {
			t.Fatalf("Put %s: %v", pair[0], err)
		}
	}
	gResp, err := kv.Get(context.Background(), &kvv1.GetRequest{Key: []byte("b")})
	if err != nil || string(gResp.Value) != "2" {
		t.Fatalf("Get b = (%q, %v), want (\"2\", nil)", gResp.Value, err)
	}

	// Scan with pagination.
	var scanned []string
	var start []byte
	for {
		sResp, err := kv.Scan(context.Background(), &kvv1.ScanRequest{
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
	if got := strings.Join(scanned, ","); got != "a=1,b=2,c=3" {
		t.Fatalf("scanned = %q, want %q", got, "a=1,b=2,c=3")
	}

	if _, err := kv.Delete(context.Background(), &kvv1.DeleteRequest{Key: []byte("b")}); err != nil {
		t.Fatalf("Delete b: %v", err)
	}
	gResp, err = kv.Get(context.Background(), &kvv1.GetRequest{Key: []byte("b")})
	if err != nil || gResp.Value != nil {
		t.Fatalf("Get deleted b = (%v, %v), want (nil, nil)", gResp.Value, err)
	}

	if _, err := kv.Get(context.Background(), &kvv1.GetRequest{Key: nil}); status.Code(err) != codes.InvalidArgument {
		t.Fatalf("empty key err = %v, want InvalidArgument", err)
	}

	// Scrape /metrics and check 404 on unknown paths.
	metricsURL := "http://" + metricsLis.Addr().String()
	httpResp, err := http.Get(metricsURL + "/metrics")
	if err != nil {
		t.Fatalf("GET /metrics: %v", err)
	}
	body, _ := io.ReadAll(httpResp.Body)
	_ = httpResp.Body.Close()
	if httpResp.StatusCode != http.StatusOK {
		t.Fatalf("GET /metrics status = %d, want 200", httpResp.StatusCode)
	}
	text := string(body)
	for _, want := range []string{
		`peppydb_gateway_rpc_requests_total{code="ok",method="put"} 3`,
		`peppydb_gateway_rpc_requests_total{code="invalidargument",method="get"} 1`,
		`peppydb_gateway_rpc_duration_seconds_count{method="scan"} 2`,
		`peppydb_gateway_leader_probes_total`,
		`go_goroutines`,
	} {
		if !strings.Contains(text, want) {
			t.Errorf("/metrics missing %q", want)
		}
	}

	otherResp, err := http.Get(metricsURL + "/other")
	if err != nil {
		t.Fatalf("GET /other: %v", err)
	}
	_ = otherResp.Body.Close()
	if otherResp.StatusCode != http.StatusNotFound {
		t.Fatalf("GET /other status = %d, want 404", otherResp.StatusCode)
	}

	// Graceful shutdown.
	cancel()
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("Serve returned error: %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("Serve did not stop cleanly")
	}
}
