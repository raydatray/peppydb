package lincheck

import (
	"bufio"
	"bytes"
	"context"
	"errors"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"

	kvv1 "peppydb/proto/kv/v1/go"
)

// FindBinary resolves a binary path from an environment variable (such as
// PEPPYDB_BIN set by Buck2 `$(location ...)`). If the path is relative and not
// found from cwd, it walks up parent directories to find the workspace root.
func FindBinary(envVar string) (string, bool) {
	raw := strings.TrimSpace(os.Getenv(envVar))
	if raw == "" {
		return "", false
	}
	if filepath.IsAbs(raw) {
		if _, err := os.Stat(raw); err == nil {
			return raw, true
		}
		return "", false
	}
	if abs, err := filepath.Abs(raw); err == nil {
		if _, err := os.Stat(abs); err == nil {
			return abs, true
		}
	}
	dir, err := os.Getwd()
	if err != nil {
		return "", false
	}
	for {
		candidate := filepath.Join(dir, raw)
		if _, err := os.Stat(candidate); err == nil {
			return candidate, true
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return "", false
		}
		dir = parent
	}
}

// ClusterConfig configures a multi-process peppydb cluster for testing.
type ClusterConfig struct {
	ServerBin         string
	GatewayBin        string
	Nodes             int
	BaseDir           string
	SnapshotAfter     uint64
	KeepBelowSnapshot uint64
}

type nodeProc struct {
	id      uint64
	addr    string
	dataDir string

	mu     sync.Mutex
	cmd    *exec.Cmd
	exited chan struct{}
	stderr *lockedBuffer
}

type lockedBuffer struct {
	mu  sync.Mutex
	buf bytes.Buffer
}

func (b *lockedBuffer) Write(p []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buf.Write(p)
}

func (b *lockedBuffer) String() string {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buf.String()
}

// Cluster manages a local static cluster of real peppydb server processes
// (and optional peppydb-gateway processes).
type Cluster struct {
	cfg       ClusterConfig
	peersSpec string

	mu       sync.Mutex
	nodes    map[uint64]*nodeProc
	gateways []*GatewayProcess
}

// StartCluster allocates free loopback ports and spawns cfg.Nodes peppydb
// serve processes, waiting until every process is listening.
func StartCluster(ctx context.Context, cfg ClusterConfig) (*Cluster, error) {
	if cfg.Nodes <= 0 {
		return nil, errors.New("lincheck: Nodes must be positive")
	}
	if cfg.ServerBin == "" {
		bin, ok := FindBinary("PEPPYDB_BIN")
		if !ok {
			return nil, errors.New("lincheck: PEPPYDB_BIN not set")
		}
		cfg.ServerBin = bin
	}
	if cfg.BaseDir == "" {
		return nil, errors.New("lincheck: BaseDir is required")
	}
	addrs, err := freeLoopbackAddrs(cfg.Nodes)
	if err != nil {
		return nil, err
	}
	var peerParts []string
	for i, addr := range addrs {
		peerParts = append(peerParts, fmt.Sprintf("%d=%s", i+1, addr))
	}
	c := &Cluster{
		cfg:       cfg,
		peersSpec: strings.Join(peerParts, ","),
		nodes:     make(map[uint64]*nodeProc, cfg.Nodes),
	}
	for i, addr := range addrs {
		id := uint64(i + 1)
		dir := filepath.Join(cfg.BaseDir, fmt.Sprintf("node-%d", id))
		if err := os.MkdirAll(dir, 0o755); err != nil {
			c.Close()
			return nil, err
		}
		np := &nodeProc{id: id, addr: addr, dataDir: dir}
		c.nodes[id] = np
		bootstrap := cfg.Nodes > 1 && id == 1
		if err := c.startNode(ctx, np, bootstrap); err != nil {
			c.Close()
			return nil, err
		}
	}
	return c, nil
}

func (c *Cluster) startNode(ctx context.Context, np *nodeProc, bootstrap bool) error {
	args := []string{
		"serve",
		"--data-dir", np.dataDir,
		"--listen", np.addr,
		"--node-id", strconv.FormatUint(np.id, 10),
	}
	if c.cfg.Nodes > 1 {
		args = append(args, "--peers", c.peersSpec)
		if bootstrap {
			args = append(args, "--bootstrap")
		}
	}
	if c.cfg.SnapshotAfter > 0 {
		args = append(args, "--snapshot-after", strconv.FormatUint(c.cfg.SnapshotAfter, 10))
	}
	if c.cfg.KeepBelowSnapshot > 0 {
		args = append(args, "--keep-below-snapshot", strconv.FormatUint(c.cfg.KeepBelowSnapshot, 10))
	}

	cmd := exec.Command(c.cfg.ServerBin, args...)
	cmd.Env = append(os.Environ(), "RUST_LOG=info")
	stderrPipe, err := cmd.StderrPipe()
	if err != nil {
		return err
	}
	if err := cmd.Start(); err != nil {
		return fmt.Errorf("start node %d: %w", np.id, err)
	}

	logBuf := &lockedBuffer{}
	listening := make(chan string, 1)
	go func() {
		sc := bufio.NewScanner(stderrPipe)
		for sc.Scan() {
			line := sc.Text()
			_, _ = logBuf.Write([]byte(line + "\n"))
			if _, after, ok := strings.Cut(line, "listening on "); ok {
				select {
				case listening <- strings.TrimSpace(after):
				default:
				}
			}
		}
	}()

	exited := make(chan struct{})
	go func() {
		_ = cmd.Wait()
		close(exited)
	}()

	np.mu.Lock()
	np.cmd = cmd
	np.exited = exited
	np.stderr = logBuf
	np.mu.Unlock()

	select {
	case <-listening:
		return nil
	case <-exited:
		return fmt.Errorf("node %d exited before listening:\n%s", np.id, logBuf.String())
	case <-ctx.Done():
		_ = cmd.Process.Kill()
		<-exited
		return fmt.Errorf("timeout waiting for node %d: %w\nstderr:\n%s", np.id, ctx.Err(), logBuf.String())
	}
}

// Addrs returns the sorted list of node addresses (node 1..N).
func (c *Cluster) Addrs() []string {
	out := make([]string, 0, c.cfg.Nodes)
	for id := uint64(1); id <= uint64(c.cfg.Nodes); id++ {
		out = append(out, c.nodes[id].addr)
	}
	return out
}

// Addr returns node id's client gRPC address.
func (c *Cluster) Addr(id uint64) string {
	return c.nodes[id].addr
}

// NodeStderr returns the captured stderr of node id.
func (c *Cluster) NodeStderr(id uint64) string {
	np := c.nodes[id]
	np.mu.Lock()
	buf := np.stderr
	np.mu.Unlock()
	if buf == nil {
		return ""
	}
	return buf.String()
}

// WaitLeader polls AdminService.Status across nodes until a node (other than
// excludeID, if non-zero) reports role == "leader".
func (c *Cluster) WaitLeader(ctx context.Context, excludeID uint64) (uint64, string, error) {
	ticker := time.NewTicker(50 * time.Millisecond)
	defer ticker.Stop()
	for {
		for id := uint64(1); id <= uint64(c.cfg.Nodes); id++ {
			if id == excludeID {
				continue
			}
			addr := c.nodes[id].addr
			st, err := queryStatus(ctx, addr)
			if err == nil && st.GetRole() == "leader" {
				return id, addr, nil
			}
		}
		select {
		case <-ctx.Done():
			return 0, "", fmt.Errorf("wait leader (exclude=%d): %w", excludeID, ctx.Err())
		case <-ticker.C:
		}
	}
}

// WaitFollowerKnowsLeader waits until followerID reports the given leaderID.
func (c *Cluster) WaitFollowerKnowsLeader(ctx context.Context, followerID, leaderID uint64) error {
	ticker := time.NewTicker(50 * time.Millisecond)
	defer ticker.Stop()
	addr := c.nodes[followerID].addr
	for {
		st, err := queryStatus(ctx, addr)
		if err == nil && st.GetRole() == "follower" && st.LeaderId != nil && st.GetLeaderId() == leaderID {
			return nil
		}
		select {
		case <-ctx.Done():
			return fmt.Errorf("wait follower %d to know leader %d: %w", followerID, leaderID, ctx.Err())
		case <-ticker.C:
		}
	}
}

func queryStatus(ctx context.Context, addr string) (*kvv1.StatusResponse, error) {
	callCtx, cancel := context.WithTimeout(ctx, 500*time.Millisecond)
	defer cancel()
	conn, err := grpc.NewClient(addr, grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		return nil, err
	}
	defer func() { _ = conn.Close() }()
	return kvv1.NewAdminServiceClient(conn).Status(callCtx, &kvv1.StatusRequest{})
}

// Kill terminates node id immediately with SIGKILL and waits for exit.
func (c *Cluster) Kill(id uint64) error {
	np, ok := c.nodes[id]
	if !ok {
		return fmt.Errorf("unknown node %d", id)
	}
	np.mu.Lock()
	cmd := np.cmd
	exited := np.exited
	np.cmd = nil
	np.mu.Unlock()
	if cmd == nil {
		return nil
	}
	_ = cmd.Process.Kill()
	<-exited
	return nil
}

// Stop sends SIGTERM to node id and waits for graceful exit.
func (c *Cluster) Stop(id uint64) error {
	np, ok := c.nodes[id]
	if !ok {
		return fmt.Errorf("unknown node %d", id)
	}
	np.mu.Lock()
	cmd := np.cmd
	exited := np.exited
	np.cmd = nil
	np.mu.Unlock()
	if cmd == nil {
		return nil
	}
	_ = cmd.Process.Signal(syscall.SIGTERM)
	select {
	case <-exited:
	case <-time.After(10 * time.Second):
		_ = cmd.Process.Kill()
		<-exited
	}
	return nil
}

// Restart starts a previously stopped or killed node with its existing data dir.
func (c *Cluster) Restart(ctx context.Context, id uint64) error {
	np, ok := c.nodes[id]
	if !ok {
		return fmt.Errorf("unknown node %d", id)
	}
	return c.startNode(ctx, np, false)
}

// GatewayProcess represents a running peppydb-gateway child process.
type GatewayProcess struct {
	addr        string
	metricsAddr string
	cmd         *exec.Cmd
	exited      chan struct{}
	stderr      *lockedBuffer
	stopOnce    sync.Once
}

// Addr returns the gateway's bound gRPC listen address.
func (g *GatewayProcess) Addr() string { return g.addr }

// MetricsAddr returns the gateway's bound HTTP metrics address (empty if disabled).
func (g *GatewayProcess) MetricsAddr() string { return g.metricsAddr }

// Stderr returns the captured stderr output of the gateway process.
func (g *GatewayProcess) Stderr() string { return g.stderr.String() }

// Stop sends SIGTERM to the gateway and waits for exit.
func (g *GatewayProcess) Stop() {
	g.stopOnce.Do(func() {
		if g.cmd != nil && g.cmd.Process != nil {
			_ = g.cmd.Process.Signal(syscall.SIGTERM)
			select {
			case <-g.exited:
			case <-time.After(5 * time.Second):
				_ = g.cmd.Process.Kill()
				<-g.exited
			}
		}
	})
}

// StartGateway spawns a peppydb-gateway process pointed at backends.
func (c *Cluster) StartGateway(ctx context.Context, backends []string, withMetrics bool) (*GatewayProcess, error) {
	bin := c.cfg.GatewayBin
	if bin == "" {
		found, ok := FindBinary("PEPPYDB_GATEWAY_BIN")
		if !ok {
			return nil, errors.New("lincheck: PEPPYDB_GATEWAY_BIN not set")
		}
		bin = found
	}
	args := []string{
		"--listen", "127.0.0.1:0",
		"--backends", strings.Join(backends, ","),
	}
	if withMetrics {
		args = append(args, "--metrics-listen", "127.0.0.1:0")
	}
	cmd := exec.Command(bin, args...)
	stderrPipe, err := cmd.StderrPipe()
	if err != nil {
		return nil, err
	}
	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("start gateway: %w", err)
	}

	logBuf := &lockedBuffer{}
	grpcReady := make(chan string, 1)
	metricsReady := make(chan string, 1)
	go func() {
		sc := bufio.NewScanner(stderrPipe)
		for sc.Scan() {
			line := sc.Text()
			_, _ = logBuf.Write([]byte(line + "\n"))
			if _, after, ok := strings.Cut(line, "metrics listening on "); ok {
				select {
				case metricsReady <- strings.TrimSpace(after):
				default:
				}
			} else if _, after, ok := strings.Cut(line, "listening on "); ok {
				select {
				case grpcReady <- strings.TrimSpace(after):
				default:
				}
			}
		}
	}()

	exited := make(chan struct{})
	go func() {
		_ = cmd.Wait()
		close(exited)
	}()

	var grpcAddr, metricsAddr string
	select {
	case grpcAddr = <-grpcReady:
	case <-exited:
		return nil, fmt.Errorf("gateway exited early:\n%s", logBuf.String())
	case <-ctx.Done():
		_ = cmd.Process.Kill()
		<-exited
		return nil, fmt.Errorf("timeout waiting for gateway: %w\nstderr:\n%s", ctx.Err(), logBuf.String())
	}
	if withMetrics {
		select {
		case metricsAddr = <-metricsReady:
		case <-exited:
			return nil, fmt.Errorf("gateway exited before metrics ready:\n%s", logBuf.String())
		case <-ctx.Done():
			_ = cmd.Process.Kill()
			<-exited
			return nil, fmt.Errorf("timeout waiting for gateway metrics: %w\nstderr:\n%s", ctx.Err(), logBuf.String())
		}
	}

	gw := &GatewayProcess{
		addr:        grpcAddr,
		metricsAddr: metricsAddr,
		cmd:         cmd,
		exited:      exited,
		stderr:      logBuf,
	}
	c.mu.Lock()
	c.gateways = append(c.gateways, gw)
	c.mu.Unlock()
	return gw, nil
}

// Close stops all gateway and server processes managed by the cluster.
func (c *Cluster) Close() {
	c.mu.Lock()
	gws := c.gateways
	c.gateways = nil
	c.mu.Unlock()
	for _, gw := range gws {
		gw.Stop()
	}
	for id := uint64(1); id <= uint64(c.cfg.Nodes); id++ {
		if np := c.nodes[id]; np != nil {
			_ = c.Kill(id)
		}
	}
}

func freeLoopbackAddrs(n int) ([]string, error) {
	listeners := make([]net.Listener, 0, n)
	addrs := make([]string, 0, n)
	for range n {
		l, err := net.Listen("tcp", "127.0.0.1:0")
		if err != nil {
			for _, prev := range listeners {
				_ = prev.Close()
			}
			return nil, err
		}
		listeners = append(listeners, l)
		addrs = append(addrs, l.Addr().String())
	}
	for _, l := range listeners {
		_ = l.Close()
	}
	return addrs, nil
}
