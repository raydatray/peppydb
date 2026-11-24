// Package kvclient implements a leader-tracking gRPC client for one peppydb
// Raft group (plan 0033).
//
// It maintains a pool of persistent gRPC connections to peer addresses, caches
// the current leader address, follows UNAVAILABLE LeaderHint trailers, and
// preserves strict linearizability for writes by never retrying a write whose
// outcome is unknown (UNAVAILABLE without a LeaderHint).
package kvclient

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/status"

	kvv1 "peppydb/proto/kv/v1/go"
)

const (
	defaultMaxRedirects   = 5
	defaultMaxReadRetries = 5
	defaultRetryBackoff   = 25 * time.Millisecond
	maxRetryBackoff       = 250 * time.Millisecond
	defaultProbeTimeout   = 2 * time.Second
)

// LeaderDiscovery locates the current Raft leader address when the cached
// leader is empty or invalidated. Phase 2 probes AdminService.Status across
// known peers; phase 3 can supply a placement-driver cache and push updates
// via Client.SetLeader.
type LeaderDiscovery interface {
	Discover(ctx context.Context) (string, error)
}

// Hooks receives optional notifications for metrics and diagnostics.
type Hooks struct {
	OnRedirect  func()
	OnReadRetry func()
	OnProbe     func()
}

// Options configures a Client for one Raft group.
type Options struct {
	// Seeds lists initial peer addresses (`host:port`). At least one seed is
	// required unless a custom Discovery is supplied.
	Seeds []string
	// Discovery resolves the leader address when the cache is empty or stale.
	// When nil, Client probes AdminService.Status across known peers.
	Discovery LeaderDiscovery
	// DialOptions overrides the gRPC dial options. Defaults to insecure creds.
	DialOptions []grpc.DialOption
	// MaxRedirects caps consecutive LeaderHint redirects per RPC. Defaults to 5.
	MaxRedirects int
	// MaxReadRetries caps non-hint UNAVAILABLE retries for reads. Defaults to 5.
	MaxReadRetries int
	// RetryBackoff is the initial backoff before a read retry. Defaults to 25ms.
	RetryBackoff time.Duration
	// ProbeTimeout bounds one discovery call. Defaults to 2s.
	ProbeTimeout time.Duration
	// Hooks are optional counters/callbacks invoked on redirects, retries, and probes.
	Hooks Hooks
}

// Client talks to one peppydb Raft group over persistent peer connections.
type Client struct {
	opts      Options
	seeds     []string
	discovery LeaderDiscovery
	dialOpts  []grpc.DialOption

	bgCtx    context.Context
	bgCancel context.CancelFunc

	mu     sync.RWMutex
	closed bool
	leader string
	conns  map[string]*grpc.ClientConn
	known  []string
	knownM map[string]struct{}

	// Coalesces concurrent discovery probes so a failover does not stampede peers.
	probeMu   sync.Mutex
	probeCh   chan struct{}
	probeAddr string
	probeErr  error

	rr atomic.Uint64
}

// New constructs a Client for one Raft group.
func New(opts Options) (*Client, error) {
	seeds := cleanAddrs(opts.Seeds)
	if len(seeds) == 0 && opts.Discovery == nil {
		return nil, errors.New("kvclient: at least one seed address or Discovery is required")
	}
	if opts.MaxRedirects <= 0 {
		opts.MaxRedirects = defaultMaxRedirects
	}
	if opts.MaxReadRetries <= 0 {
		opts.MaxReadRetries = defaultMaxReadRetries
	}
	if opts.RetryBackoff <= 0 {
		opts.RetryBackoff = defaultRetryBackoff
	}
	if opts.ProbeTimeout <= 0 {
		opts.ProbeTimeout = defaultProbeTimeout
	}
	dialOpts := opts.DialOptions
	if len(dialOpts) == 0 {
		dialOpts = []grpc.DialOption{grpc.WithTransportCredentials(insecure.NewCredentials())}
	}
	bgCtx, bgCancel := context.WithCancel(context.Background())
	c := &Client{
		opts:     opts,
		seeds:    seeds,
		dialOpts: dialOpts,
		bgCtx:    bgCtx,
		bgCancel: bgCancel,
		conns:    make(map[string]*grpc.ClientConn),
		known:    append([]string(nil), seeds...),
		knownM:   make(map[string]struct{}, len(seeds)),
	}
	for _, s := range seeds {
		c.knownM[s] = struct{}{}
	}
	if opts.Discovery != nil {
		c.discovery = opts.Discovery
	} else {
		c.discovery = statusDiscovery{client: c}
	}
	return c, nil
}

// ExtractLeaderHint decodes a kvv1.LeaderHint from a gRPC status's standard
// google.rpc.Status details.
func ExtractLeaderHint(err error) string {
	st, ok := status.FromError(err)
	if !ok {
		return ""
	}
	for _, detail := range st.Details() {
		if hint, ok := detail.(*kvv1.LeaderHint); ok && hint.LeaderAddr != "" {
			return hint.LeaderAddr
		}
	}
	return ""
}

// Leader returns the currently cached leader address, or "" if unknown.
func (c *Client) Leader() string {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return c.leader
}

// SetLeader updates the cached leader address (e.g. from a placement-driver watch).
func (c *Client) SetLeader(addr string) {
	addr = strings.TrimSpace(addr)
	if addr == "" {
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	c.leader = addr
	c.recordKnownLocked(addr)
}

// Close closes all pooled gRPC connections and stops background probes.
func (c *Client) Close() error {
	c.bgCancel()
	c.mu.Lock()
	if c.closed {
		c.mu.Unlock()
		return nil
	}
	c.closed = true
	conns := make([]*grpc.ClientConn, 0, len(c.conns))
	for _, conn := range c.conns {
		conns = append(conns, conn)
	}
	c.conns = nil
	c.mu.Unlock()

	var firstErr error
	for _, conn := range conns {
		if err := conn.Close(); err != nil && firstErr == nil {
			firstErr = err
		}
	}
	return firstErr
}

// Get sends a linearizable Get RPC, following LeaderHint redirects and retrying
// transient non-hint UNAVAILABLE failures up to MaxReadRetries.
func (c *Client) Get(ctx context.Context, req *kvv1.GetRequest) (*kvv1.GetResponse, error) {
	return doRead(ctx, c, func(ctx context.Context, kv kvv1.KvServiceClient, opts ...grpc.CallOption) (*kvv1.GetResponse, error) {
		return kv.Get(ctx, req, opts...)
	})
}

// Scan sends a Scan RPC with the same read redirect and retry rules as Get.
func (c *Client) Scan(ctx context.Context, req *kvv1.ScanRequest) (*kvv1.ScanResponse, error) {
	return doRead(ctx, c, func(ctx context.Context, kv kvv1.KvServiceClient, opts ...grpc.CallOption) (*kvv1.ScanResponse, error) {
		return kv.Scan(ctx, req, opts...)
	})
}

// Put sends a Put RPC. It follows UNAVAILABLE LeaderHint redirects (where the
// follower guaranteed nothing was proposed), and propagates non-hint
// UNAVAILABLE without retrying so unknown-outcome writes are never duplicated.
func (c *Client) Put(ctx context.Context, req *kvv1.PutRequest) (*kvv1.PutResponse, error) {
	return doWrite(ctx, c, func(ctx context.Context, kv kvv1.KvServiceClient, opts ...grpc.CallOption) (*kvv1.PutResponse, error) {
		return kv.Put(ctx, req, opts...)
	})
}

// Delete sends a Delete RPC with the same linearizability-safe write rules as Put.
func (c *Client) Delete(ctx context.Context, req *kvv1.DeleteRequest) (*kvv1.DeleteResponse, error) {
	return doWrite(ctx, c, func(ctx context.Context, kv kvv1.KvServiceClient, opts ...grpc.CallOption) (*kvv1.DeleteResponse, error) {
		return kv.Delete(ctx, req, opts...)
	})
}

func doRead[Resp any](
	ctx context.Context,
	c *Client,
	call func(context.Context, kvv1.KvServiceClient, ...grpc.CallOption) (Resp, error),
) (Resp, error) {
	var zero Resp
	redirects := 0
	retries := 0
	for {
		if err := ctx.Err(); err != nil {
			return zero, status.FromContextError(err).Err()
		}
		target, err := c.pickTarget(ctx)
		if err != nil {
			return zero, err
		}
		kv, err := c.kvClient(target)
		if err != nil {
			return zero, err
		}
		resp, err := call(ctx, kv)
		if err == nil {
			c.SetLeader(target)
			return resp, nil
		}
		if status.Code(err) != codes.Unavailable {
			return zero, err
		}
		if hint := ExtractLeaderHint(err); hint != "" {
			c.SetLeader(hint)
			if c.opts.Hooks.OnRedirect != nil {
				c.opts.Hooks.OnRedirect()
			}
			if redirects >= c.opts.MaxRedirects {
				return zero, err
			}
			redirects++
			continue
		}
		c.invalidateLeader(target)
		if retries >= c.opts.MaxReadRetries {
			return zero, err
		}
		if c.opts.Hooks.OnReadRetry != nil {
			c.opts.Hooks.OnReadRetry()
		}
		if err := sleepCtx(ctx, c.backoff(retries)); err != nil {
			return zero, status.FromContextError(err).Err()
		}
		_, _ = c.discoverLeader(ctx)
		retries++
	}
}

func doWrite[Resp any](
	ctx context.Context,
	c *Client,
	call func(context.Context, kvv1.KvServiceClient, ...grpc.CallOption) (Resp, error),
) (Resp, error) {
	var zero Resp
	redirects := 0
	for {
		if err := ctx.Err(); err != nil {
			return zero, status.FromContextError(err).Err()
		}
		target, err := c.pickTarget(ctx)
		if err != nil {
			return zero, err
		}
		kv, err := c.kvClient(target)
		if err != nil {
			return zero, err
		}
		resp, err := call(ctx, kv)
		if err == nil {
			c.SetLeader(target)
			return resp, nil
		}
		if status.Code(err) != codes.Unavailable {
			return zero, err
		}
		if hint := ExtractLeaderHint(err); hint != "" {
			c.SetLeader(hint)
			if c.opts.Hooks.OnRedirect != nil {
				c.opts.Hooks.OnRedirect()
			}
			if redirects >= c.opts.MaxRedirects {
				return zero, err
			}
			redirects++
			continue
		}
		// Non-hint UNAVAILABLE: the write may have been proposed before a
		// leadership change or crash. Do not retry; refresh leader cache for
		// subsequent requests and surface UNAVAILABLE to the caller.
		c.invalidateLeader(target)
		c.triggerBackgroundProbe()
		return zero, err
	}
}

func (c *Client) pickTarget(ctx context.Context) (string, error) {
	if addr := c.Leader(); addr != "" {
		return addr, nil
	}
	if addr, err := c.discoverLeader(ctx); err == nil && addr != "" {
		return addr, nil
	}
	// Fallback to round-robin over known peers so a follower can answer with
	// a LeaderHint if discovery raced with an election.
	peers := c.knownPeers()
	if len(peers) == 0 {
		return "", status.Error(codes.Unavailable, "no backend peers available")
	}
	idx := c.rr.Add(1) - 1
	return peers[idx%uint64(len(peers))], nil
}

func (c *Client) invalidateLeader(stale string) {
	c.mu.Lock()
	if c.leader == stale {
		c.leader = ""
	}
	c.mu.Unlock()
}

func (c *Client) knownPeers() []string {
	c.mu.RLock()
	defer c.mu.RUnlock()
	out := make([]string, len(c.known))
	copy(out, c.known)
	return out
}

func (c *Client) recordKnown(addr string) {
	addr = strings.TrimSpace(addr)
	if addr == "" {
		return
	}
	c.mu.Lock()
	c.recordKnownLocked(addr)
	c.mu.Unlock()
}

func (c *Client) recordKnownLocked(addr string) {
	if _, ok := c.knownM[addr]; !ok {
		c.knownM[addr] = struct{}{}
		c.known = append(c.known, addr)
	}
}

func (c *Client) connFor(addr string) (*grpc.ClientConn, error) {
	c.mu.RLock()
	if c.closed {
		c.mu.RUnlock()
		return nil, status.Error(codes.Unavailable, "kvclient closed")
	}
	if conn, ok := c.conns[addr]; ok {
		c.mu.RUnlock()
		return conn, nil
	}
	c.mu.RUnlock()

	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return nil, status.Error(codes.Unavailable, "kvclient closed")
	}
	if conn, ok := c.conns[addr]; ok {
		return conn, nil
	}
	conn, err := grpc.NewClient(addr, c.dialOpts...)
	if err != nil {
		return nil, status.Errorf(codes.Unavailable, "dial %s: %v", addr, err)
	}
	c.conns[addr] = conn
	c.recordKnownLocked(addr)
	return conn, nil
}

func (c *Client) kvClient(addr string) (kvv1.KvServiceClient, error) {
	conn, err := c.connFor(addr)
	if err != nil {
		return nil, err
	}
	return kvv1.NewKvServiceClient(conn), nil
}

// discoverLeader runs a coalesced discovery call so concurrent callers share
// a single in-flight probe.
func (c *Client) discoverLeader(ctx context.Context) (string, error) {
	ch := c.startOrJoinProbe()
	select {
	case <-ch:
		c.probeMu.Lock()
		addr, err := c.probeAddr, c.probeErr
		c.probeMu.Unlock()
		return addr, err
	case <-ctx.Done():
		return "", ctx.Err()
	}
}

func (c *Client) triggerBackgroundProbe() {
	_ = c.startOrJoinProbe()
}

func (c *Client) startOrJoinProbe() <-chan struct{} {
	c.probeMu.Lock()
	if c.probeCh != nil {
		ch := c.probeCh
		c.probeMu.Unlock()
		return ch
	}
	ch := make(chan struct{})
	c.probeCh = ch
	c.probeMu.Unlock()

	go func() {
		if c.opts.Hooks.OnProbe != nil {
			c.opts.Hooks.OnProbe()
		}
		probeCtx, cancel := context.WithTimeout(c.bgCtx, c.opts.ProbeTimeout)
		addr, err := c.discovery.Discover(probeCtx)
		cancel()
		if err == nil && addr != "" {
			c.SetLeader(addr)
		}
		c.probeMu.Lock()
		c.probeAddr = addr
		c.probeErr = err
		c.probeCh = nil
		close(ch)
		c.probeMu.Unlock()
	}()
	return ch
}

func (c *Client) backoff(attempt int) time.Duration {
	d := c.opts.RetryBackoff
	for range attempt {
		if d >= maxRetryBackoff {
			return maxRetryBackoff
		}
		d *= 2
	}
	if d > maxRetryBackoff {
		return maxRetryBackoff
	}
	return d
}

func sleepCtx(ctx context.Context, d time.Duration) error {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-t.C:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

type statusDiscovery struct {
	client *Client
}

func (d statusDiscovery) Discover(ctx context.Context) (string, error) {
	peers := d.client.knownPeers()
	var hinted string
	var lastErr error
	for _, addr := range peers {
		conn, err := d.client.connFor(addr)
		if err != nil {
			lastErr = err
			continue
		}
		resp, err := kvv1.NewAdminServiceClient(conn).Status(ctx, &kvv1.StatusRequest{})
		if err != nil {
			lastErr = err
			continue
		}
		// Learn dynamically added voters so probes keep working even if
		// original seed nodes are later removed from the cluster.
		for _, voter := range resp.GetVoters() {
			d.client.recordKnown(voter.GetAddr())
		}
		if resp.GetRole() == "leader" {
			if resp.GetLeaderAddr() != "" {
				return resp.GetLeaderAddr(), nil
			}
			return addr, nil
		}
		if hinted == "" && resp.GetLeaderAddr() != "" {
			hinted = resp.GetLeaderAddr()
		}
	}
	if hinted != "" {
		return hinted, nil
	}
	if lastErr != nil {
		return "", fmt.Errorf("discover leader: %w", lastErr)
	}
	return "", errors.New("discover leader: no leader elected")
}

func cleanAddrs(in []string) []string {
	seen := make(map[string]struct{}, len(in))
	out := make([]string, 0, len(in))
	for _, raw := range in {
		addr := strings.TrimSpace(raw)
		if addr == "" {
			continue
		}
		if _, dup := seen[addr]; dup {
			continue
		}
		seen[addr] = struct{}{}
		out = append(out, addr)
	}
	return out
}
