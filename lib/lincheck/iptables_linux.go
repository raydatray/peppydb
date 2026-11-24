//go:build linux

package lincheck

import (
	"context"
	"fmt"
	"math/rand/v2"
	"net"
	"os/exec"
	"time"
)

// PartitionGuard drops traffic between distinct loopback IPs via Linux
// iptables and removes all installed rules on Heal.
type PartitionGuard struct {
	rules [][]string
}

// IsolateNode installs bidirectional DROP rules on loopback between nodeAddr's
// IP and every peer IP in peerAddrs.
func IsolateNode(nodeAddr string, peerAddrs []string) (*PartitionGuard, error) {
	nodeIP, _, err := net.SplitHostPort(nodeAddr)
	if err != nil {
		return nil, fmt.Errorf("split node addr %q: %w", nodeAddr, err)
	}
	g := &PartitionGuard{}
	for _, peerAddr := range peerAddrs {
		peerIP, _, err := net.SplitHostPort(peerAddr)
		if err != nil {
			g.Heal()
			return nil, fmt.Errorf("split peer addr %q: %w", peerAddr, err)
		}
		if peerIP == nodeIP {
			g.Heal()
			return nil, fmt.Errorf("cannot isolate identical IPs (%s); configure ClusterConfig.BindIPs", nodeIP)
		}
		for _, pair := range [][2]string{{nodeIP, peerIP}, {peerIP, nodeIP}} {
			rule := []string{"-o", "lo", "-s", pair[0], "-d", pair[1], "-j", "DROP"}
			args := append([]string{"-A", "OUTPUT"}, rule...)
			if out, err := exec.Command("iptables", args...).CombinedOutput(); err != nil {
				g.Heal()
				return nil, fmt.Errorf("iptables %v: %w (%s)", args, err, string(out))
			}
			g.rules = append(g.rules, rule)
		}
	}
	return g, nil
}

// Heal removes every iptables rule installed by this guard.
func (g *PartitionGuard) Heal() {
	for _, rule := range g.rules {
		args := append([]string{"-D", "OUTPUT"}, rule...)
		_ = exec.Command("iptables", args...).Run()
	}
	g.rules = nil
}

// IPTablesPartitionFault isolates the current leader (or a random voter) from
// its peers using iptables, waits for majority failover, then heals and
// confirms catch-up. Requires CAP_NET_ADMIN (run under tools/linux-run).
func IPTablesPartitionFault(
	ctx context.Context,
	cluster *Cluster,
	voters []uint64,
	rng *rand.Rand,
) (string, error) {
	leaderID, _, err := cluster.WaitLeader(ctx, 0)
	if err != nil {
		return "", err
	}
	victim := leaderID
	if rng.IntN(3) == 0 && len(voters) > 1 {
		victim = voters[rng.IntN(len(voters))]
	}
	var peerAddrs []string
	for _, id := range voters {
		if id != victim {
			peerAddrs = append(peerAddrs, cluster.Addr(id))
		}
	}
	guard, err := IsolateNode(cluster.Addr(victim), peerAddrs)
	if err != nil {
		return "", err
	}
	defer guard.Heal()

	if victim == leaderID {
		if _, _, err := cluster.WaitLeader(ctx, victim); err != nil {
			return "", fmt.Errorf("majority failover during partition of leader %d: %w", victim, err)
		}
	}
	time.Sleep(800 * time.Millisecond)
	guard.Heal()

	if err := waitNodeCaughtUp(ctx, cluster, victim); err != nil {
		return "", fmt.Errorf("healed node %d catch-up: %w", victim, err)
	}
	return fmt.Sprintf("isolated node %d and healed", victim), nil
}
