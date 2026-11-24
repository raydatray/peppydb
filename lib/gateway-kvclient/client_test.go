package kvclient

import (
	"context"
	"net"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	kvv1 "peppydb/proto/kv/v1/go"
)

// fakeNode simulates a peppydb server's KvService and AdminService in-process.
type fakeNode struct {
	kvv1.UnimplementedKvServiceServer
	kvv1.UnimplementedAdminServiceServer

	addr string
	srv  *grpc.Server

	mu            sync.Mutex
	role          string
	leaderAddr    string
	leaderID      uint64
	kv            map[string][]byte
	failNextReads int
	failNextWrite error
	getCalls      atomic.Int64
	putCalls      atomic.Int64
	deleteCalls   atomic.Int64
	scanCalls     atomic.Int64
	statusCalls   atomic.Int64
}

func startFakeNode(t *testing.T, role string) *fakeNode {
	t.Helper()
	lis, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	n := &fakeNode{
		addr: lis.Addr().String(),
		srv:  grpc.NewServer(),
		role: role,
		kv:   make(map[string][]byte),
	}
	if role == "leader" {
		n.leaderAddr = n.addr
		n.leaderID = 1
	}
	kvv1.RegisterKvServiceServer(n.srv, n)
	kvv1.RegisterAdminServiceServer(n.srv, n)
	go func() {
		_ = n.srv.Serve(lis)
	}()
	t.Cleanup(n.srv.Stop)
	return n
}

func (n *fakeNode) setRole(role string, leaderID uint64, leaderAddr string) {
	n.mu.Lock()
	defer n.mu.Unlock()
	n.role = role
	n.leaderID = leaderID
	n.leaderAddr = leaderAddr
}

func (n *fakeNode) notLeaderErr(leaderID uint64, leaderAddr string) error {
	if leaderAddr != "" {
		st, err := status.Newf(codes.Unavailable, "not leader; leader is %s", leaderAddr).
			WithDetails(&kvv1.LeaderHint{
				LeaderId:   leaderID,
				LeaderAddr: leaderAddr,
			})
		if err == nil {
			return st.Err()
		}
		return status.Errorf(codes.Unavailable, "not leader; leader is %s", leaderAddr)
	}
	return status.Error(codes.Unavailable, "not leader; no leader elected")
}

func (n *fakeNode) Status(_ context.Context, _ *kvv1.StatusRequest) (*kvv1.StatusResponse, error) {
	n.statusCalls.Add(1)
	n.mu.Lock()
	defer n.mu.Unlock()
	resp := &kvv1.StatusResponse{
		Role: n.role,
	}
	if n.leaderAddr != "" {
		id := n.leaderID
		addr := n.leaderAddr
		resp.LeaderId = &id
		resp.LeaderAddr = &addr
	}
	return resp, nil
}

func (n *fakeNode) Get(ctx context.Context, req *kvv1.GetRequest) (*kvv1.GetResponse, error) {
	n.getCalls.Add(1)
	if len(req.Key) == 0 {
		return nil, status.Error(codes.InvalidArgument, "empty key")
	}
	n.mu.Lock()
	defer n.mu.Unlock()
	if n.failNextReads > 0 {
		n.failNextReads--
		return nil, status.Error(codes.Unavailable, "transient quorum loss")
	}
	if n.role != "leader" {
		return nil, n.notLeaderErr(n.leaderID, n.leaderAddr)
	}
	val, ok := n.kv[string(req.Key)]
	if !ok {
		return &kvv1.GetResponse{}, nil
	}
	cp := append([]byte(nil), val...)
	return &kvv1.GetResponse{Value: cp}, nil
}

func (n *fakeNode) Put(ctx context.Context, req *kvv1.PutRequest) (*kvv1.PutResponse, error) {
	n.putCalls.Add(1)
	if len(req.Key) == 0 {
		return nil, status.Error(codes.InvalidArgument, "empty key")
	}
	n.mu.Lock()
	defer n.mu.Unlock()
	if n.failNextWrite != nil {
		err := n.failNextWrite
		n.failNextWrite = nil
		return nil, err
	}
	if n.role != "leader" {
		return nil, n.notLeaderErr(n.leaderID, n.leaderAddr)
	}
	n.kv[string(req.Key)] = append([]byte(nil), req.Value...)
	return &kvv1.PutResponse{}, nil
}

func (n *fakeNode) Delete(ctx context.Context, req *kvv1.DeleteRequest) (*kvv1.DeleteResponse, error) {
	n.deleteCalls.Add(1)
	if len(req.Key) == 0 {
		return nil, status.Error(codes.InvalidArgument, "empty key")
	}
	n.mu.Lock()
	defer n.mu.Unlock()
	if n.failNextWrite != nil {
		err := n.failNextWrite
		n.failNextWrite = nil
		return nil, err
	}
	if n.role != "leader" {
		return nil, n.notLeaderErr(n.leaderID, n.leaderAddr)
	}
	delete(n.kv, string(req.Key))
	return &kvv1.DeleteResponse{}, nil
}

func (n *fakeNode) Scan(ctx context.Context, _ *kvv1.ScanRequest) (*kvv1.ScanResponse, error) {
	n.scanCalls.Add(1)
	n.mu.Lock()
	defer n.mu.Unlock()
	if n.failNextReads > 0 {
		n.failNextReads--
		return nil, status.Error(codes.Unavailable, "transient quorum loss")
	}
	if n.role != "leader" {
		return nil, n.notLeaderErr(n.leaderID, n.leaderAddr)
	}
	var records []*kvv1.KeyValue
	for k, v := range n.kv {
		records = append(records, &kvv1.KeyValue{
			Key:   []byte(k),
			Value: append([]byte(nil), v...),
		})
	}
	return &kvv1.ScanResponse{Records: records}, nil
}

func TestInitialDiscoveryViaStatus(t *testing.T) {
	leader := startFakeNode(t, "leader")
	follower := startFakeNode(t, "follower")
	follower.setRole("follower", 1, leader.addr)

	var probes atomic.Int64
	c, err := New(Options{
		Seeds: []string{follower.addr},
		Hooks: Hooks{OnProbe: func() { probes.Add(1) }},
	})
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	t.Cleanup(func() { _ = c.Close() })

	ctx := context.Background()
	if _, err := c.Put(ctx, &kvv1.PutRequest{Key: []byte("k1"), Value: []byte("v1")}); err != nil {
		t.Fatalf("Put: %v", err)
	}
	if got := c.Leader(); got != leader.addr {
		t.Fatalf("cached leader = %q, want %q", got, leader.addr)
	}
	if follower.putCalls.Load() != 0 {
		t.Fatalf("follower received %d puts, want 0 (discovered leader first)", follower.putCalls.Load())
	}
	if probes.Load() != 1 {
		t.Fatalf("probes = %d, want 1", probes.Load())
	}
}

func TestLeaderHintRedirectOnWritesAndReads(t *testing.T) {
	oldLeader := startFakeNode(t, "leader")
	newLeader := startFakeNode(t, "follower")

	var redirects atomic.Int64
	c, err := New(Options{
		Seeds: []string{oldLeader.addr, newLeader.addr},
		Hooks: Hooks{OnRedirect: func() { redirects.Add(1) }},
	})
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	t.Cleanup(func() { _ = c.Close() })
	c.SetLeader(oldLeader.addr)

	// Leadership moves; cached leader is now a follower with a LeaderHint.
	oldLeader.setRole("follower", 2, newLeader.addr)
	newLeader.setRole("leader", 2, newLeader.addr)

	ctx := context.Background()
	if _, err := c.Put(ctx, &kvv1.PutRequest{Key: []byte("k"), Value: []byte("v")}); err != nil {
		t.Fatalf("Put after leadership transfer: %v", err)
	}
	if got := c.Leader(); got != newLeader.addr {
		t.Fatalf("cached leader = %q, want %q", got, newLeader.addr)
	}
	if redirects.Load() != 1 {
		t.Fatalf("redirects = %d, want 1", redirects.Load())
	}

	// Point cache back at follower to test Get redirect as well.
	c.SetLeader(oldLeader.addr)
	resp, err := c.Get(ctx, &kvv1.GetRequest{Key: []byte("k")})
	if err != nil {
		t.Fatalf("Get after redirect: %v", err)
	}
	if string(resp.Value) != "v" {
		t.Fatalf("Get value = %q, want %q", resp.Value, "v")
	}
	if redirects.Load() != 2 {
		t.Fatalf("redirects = %d, want 2", redirects.Load())
	}

	// Point cache back at follower to test Delete and Scan redirects.
	c.SetLeader(oldLeader.addr)
	if _, err := c.Scan(ctx, &kvv1.ScanRequest{}); err != nil {
		t.Fatalf("Scan after redirect: %v", err)
	}
	c.SetLeader(oldLeader.addr)
	if _, err := c.Delete(ctx, &kvv1.DeleteRequest{Key: []byte("k")}); err != nil {
		t.Fatalf("Delete after redirect: %v", err)
	}
	if redirects.Load() != 4 {
		t.Fatalf("redirects = %d, want 4", redirects.Load())
	}
}

func TestReadRetriesTransientUnavailable(t *testing.T) {
	leader := startFakeNode(t, "leader")
	leader.mu.Lock()
	leader.kv["k"] = []byte("ok")
	leader.failNextReads = 2
	leader.mu.Unlock()

	var retries atomic.Int64
	c, err := New(Options{
		Seeds:        []string{leader.addr},
		RetryBackoff: time.Millisecond,
		Hooks:        Hooks{OnReadRetry: func() { retries.Add(1) }},
	})
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	t.Cleanup(func() { _ = c.Close() })
	c.SetLeader(leader.addr)

	resp, err := c.Get(context.Background(), &kvv1.GetRequest{Key: []byte("k")})
	if err != nil {
		t.Fatalf("Get should succeed after retries: %v", err)
	}
	if string(resp.Value) != "ok" {
		t.Fatalf("value = %q, want %q", resp.Value, "ok")
	}
	if retries.Load() != 2 {
		t.Fatalf("read retries = %d, want 2", retries.Load())
	}
}

func TestWriteDoesNotRetryWithoutLeaderHint(t *testing.T) {
	oldLeader := startFakeNode(t, "leader")
	newLeader := startFakeNode(t, "leader")
	oldLeader.mu.Lock()
	oldLeader.failNextWrite = status.Error(codes.Unavailable, "write outcome unknown; retry")
	oldLeader.mu.Unlock()

	c, err := New(Options{
		Seeds:        []string{newLeader.addr, oldLeader.addr},
		RetryBackoff: time.Millisecond,
	})
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	t.Cleanup(func() { _ = c.Close() })
	c.SetLeader(oldLeader.addr)

	// Switch oldLeader to follower with no hint after the unknown-outcome failure
	// so the background probe finds newLeader.
	_, err = c.Put(context.Background(), &kvv1.PutRequest{Key: []byte("k"), Value: []byte("v1")})
	if status.Code(err) != codes.Unavailable {
		t.Fatalf("Put err = %v, want Unavailable", err)
	}
	if oldLeader.putCalls.Load() != 1 {
		t.Fatalf("oldLeader putCalls = %d, want 1 (must not retry unknown-outcome write)", oldLeader.putCalls.Load())
	}
	if newLeader.putCalls.Load() != 0 {
		t.Fatalf("newLeader putCalls = %d, want 0 (must not silently replay write)", newLeader.putCalls.Load())
	}

	// Same rule for Delete.
	oldLeader.mu.Lock()
	oldLeader.failNextWrite = status.Error(codes.Unavailable, "write outcome unknown; retry")
	oldLeader.mu.Unlock()
	c.SetLeader(oldLeader.addr)

	_, err = c.Delete(context.Background(), &kvv1.DeleteRequest{Key: []byte("k")})
	if status.Code(err) != codes.Unavailable {
		t.Fatalf("Delete err = %v, want Unavailable", err)
	}
	if oldLeader.deleteCalls.Load() != 1 {
		t.Fatalf("oldLeader deleteCalls = %d, want 1", oldLeader.deleteCalls.Load())
	}
}

func TestNonRetryableErrorsPassThroughImmediately(t *testing.T) {
	leader := startFakeNode(t, "leader")
	c, err := New(Options{Seeds: []string{leader.addr}})
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	t.Cleanup(func() { _ = c.Close() })
	c.SetLeader(leader.addr)

	_, err = c.Get(context.Background(), &kvv1.GetRequest{Key: nil})
	if status.Code(err) != codes.InvalidArgument {
		t.Fatalf("Get empty key err = %v, want InvalidArgument", err)
	}
	if leader.getCalls.Load() != 1 {
		t.Fatalf("getCalls = %d, want 1", leader.getCalls.Load())
	}
}

type blockingDiscovery struct {
	calls   atomic.Int64
	release chan struct{}
	addr    string
}

func (d *blockingDiscovery) Discover(ctx context.Context) (string, error) {
	d.calls.Add(1)
	select {
	case <-d.release:
		return d.addr, nil
	case <-ctx.Done():
		return "", ctx.Err()
	}
}

func TestCoalescedCustomDiscovery(t *testing.T) {
	leader := startFakeNode(t, "leader")
	disc := &blockingDiscovery{
		release: make(chan struct{}),
		addr:    leader.addr,
	}
	c, err := New(Options{Discovery: disc})
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	t.Cleanup(func() { _ = c.Close() })

	const workers = 8
	var wg sync.WaitGroup
	errs := make(chan error, workers)
	for range workers {
		wg.Go(func() {
			_, err := c.Get(context.Background(), &kvv1.GetRequest{Key: []byte("k")})
			errs <- err
		})
	}

	// Wait briefly so all workers join the single in-flight probe.
	time.Sleep(20 * time.Millisecond)
	close(disc.release)
	wg.Wait()
	close(errs)
	for err := range errs {
		if err != nil {
			t.Fatalf("Get: %v", err)
		}
	}
	if got := disc.calls.Load(); got != 1 {
		t.Fatalf("discovery calls = %d, want 1 (coalesced)", got)
	}
}

func TestCloseRejectsSubsequentCalls(t *testing.T) {
	leader := startFakeNode(t, "leader")
	c, err := New(Options{Seeds: []string{leader.addr}})
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	c.SetLeader(leader.addr)
	if err := c.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}
	_, err = c.Get(context.Background(), &kvv1.GetRequest{Key: []byte("k")})
	if status.Code(err) != codes.Unavailable {
		t.Fatalf("Get after Close err = %v, want Unavailable", err)
	}
}
