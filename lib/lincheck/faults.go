package lincheck

import (
	"context"
	"fmt"
	"math/rand/v2"
	"time"
)

// FaultConfig configures a seeded multi-fault schedule run concurrently with
// RunWorkload.
type FaultConfig struct {
	Seed             uint64
	Rounds           int
	EnableMembership bool
	// ExtraFault is an optional platform-specific fault (such as an iptables
	// network partition on Linux) eligible for randomized selection.
	ExtraFault func(ctx context.Context, cluster *Cluster, voters []uint64, rng *rand.Rand) (string, error)
	Logf       func(format string, args ...any)
}

func (c FaultConfig) logf(format string, args ...any) {
	if c.Logf != nil {
		c.Logf(format, args...)
	}
}

// RunFaultSchedule injects a deterministic sequence of faults (leader SIGKILL,
// follower kill + snapshot catch-up, dynamic membership rotation, and optional
// ExtraFault) against cluster while concurrent client traffic runs.
func RunFaultSchedule(ctx context.Context, cluster *Cluster, cfg FaultConfig) error {
	if cfg.Rounds <= 0 {
		cfg.Rounds = 3
	}
	rng := rand.New(rand.NewPCG(cfg.Seed, 0x5eedface))
	voters := cluster.NodeIDs()
	membershipDone := false

	// Let initial traffic warm up before injecting the first fault.
	if !sleepCtx(ctx, 400*time.Millisecond) {
		return nil
	}

	for round := range cfg.Rounds {
		if ctx.Err() != nil {
			return nil
		}
		// Guarantee at least one dynamic membership swap when enabled.
		var choice int
		switch {
		case cfg.EnableMembership && !membershipDone && round == cfg.Rounds-1:
			choice = 2
		case cfg.ExtraFault != nil:
			choice = rng.IntN(4)
		case cfg.EnableMembership:
			choice = rng.IntN(3)
		default:
			choice = rng.IntN(2)
		}

		opCtx, cancel := context.WithTimeout(context.Background(), 25*time.Second)
		var err error
		switch choice {
		case 0:
			err = faultLeaderKill(opCtx, cluster, voters, cfg)
		case 1:
			err = faultFollowerSnapshotCatchup(opCtx, cluster, voters, rng, cfg)
		case 2:
			voters, err = faultMembershipSwap(opCtx, cluster, voters, rng, cfg)
			if err == nil {
				membershipDone = true
			}
		case 3:
			var desc string
			desc, err = cfg.ExtraFault(opCtx, cluster, voters, rng)
			if err == nil {
				cfg.logf("round %d: extra fault ok (%s)", round, desc)
			}
		}
		cancel()
		if err != nil {
			return fmt.Errorf("fault round %d (choice=%d): %w", round, choice, err)
		}
		if !sleepCtx(ctx, 350*time.Millisecond) {
			return nil
		}
	}
	return nil
}

func faultLeaderKill(ctx context.Context, cluster *Cluster, voters []uint64, cfg FaultConfig) error {
	leaderID, _, err := cluster.WaitLeader(ctx, 0)
	if err != nil {
		return err
	}
	cfg.logf("fault leader-sigkill: killing leader %d (voters=%v)", leaderID, voters)
	if err := cluster.Kill(leaderID); err != nil {
		return err
	}
	newLeader, _, err := cluster.WaitLeader(ctx, leaderID)
	if err != nil {
		return fmt.Errorf("failover after killing leader %d: %w", leaderID, err)
	}
	cfg.logf("fault leader-sigkill: failed over %d -> %d", leaderID, newLeader)
	time.Sleep(400 * time.Millisecond)
	if err := cluster.Restart(ctx, leaderID); err != nil {
		return fmt.Errorf("restart killed leader %d: %w", leaderID, err)
	}
	return waitNodeCaughtUp(ctx, cluster, leaderID)
}

func faultFollowerSnapshotCatchup(
	ctx context.Context,
	cluster *Cluster,
	voters []uint64,
	rng *rand.Rand,
	cfg FaultConfig,
) error {
	leaderID, _, err := cluster.WaitLeader(ctx, 0)
	if err != nil {
		return err
	}
	var followers []uint64
	for _, id := range voters {
		if id != leaderID {
			followers = append(followers, id)
		}
	}
	if len(followers) == 0 {
		return nil
	}
	victim := followers[rng.IntN(len(followers))]
	cfg.logf("fault follower-snapshot: killing follower %d", victim)
	if err := cluster.Kill(victim); err != nil {
		return err
	}
	// Let writes accumulate and trigger snapshot compaction on the leader.
	time.Sleep(600 * time.Millisecond)
	if err := cluster.Restart(ctx, victim); err != nil {
		return fmt.Errorf("restart follower %d: %w", victim, err)
	}
	if err := waitNodeCaughtUp(ctx, cluster, victim); err != nil {
		return fmt.Errorf("follower %d catch-up: %w", victim, err)
	}
	cfg.logf("fault follower-snapshot: follower %d caught up", victim)
	return nil
}

func faultMembershipSwap(
	ctx context.Context,
	cluster *Cluster,
	voters []uint64,
	rng *rand.Rand,
	cfg FaultConfig,
) ([]uint64, error) {
	leaderID, _, err := cluster.WaitLeader(ctx, 0)
	if err != nil {
		return voters, err
	}
	var followers []uint64
	for _, id := range voters {
		if id != leaderID {
			followers = append(followers, id)
		}
	}
	if len(followers) == 0 {
		return voters, nil
	}
	dropID := followers[rng.IntN(len(followers))]

	newID, newAddr, err := cluster.AddNode(ctx)
	if err != nil {
		return voters, fmt.Errorf("add node: %w", err)
	}
	cfg.logf("fault membership-swap: adding learner %d (%s) to replace voter %d", newID, newAddr, dropID)
	if err := cluster.AddLearner(ctx, newID, newAddr); err != nil {
		return voters, fmt.Errorf("add learner %d: %w", newID, err)
	}
	if err := cluster.WaitLearnerCaughtUp(ctx, newID); err != nil {
		return voters, err
	}

	nextVoters := make([]uint64, 0, len(voters))
	for _, id := range voters {
		if id != dropID {
			nextVoters = append(nextVoters, id)
		}
	}
	nextVoters = append(nextVoters, newID)

	if err := cluster.ChangeMembership(ctx, nextVoters, false); err != nil {
		return voters, fmt.Errorf("change membership to %v: %w", nextVoters, err)
	}
	if err := cluster.TransferLeader(ctx, newID); err != nil {
		return nextVoters, fmt.Errorf("transfer leader to %d: %w", newID, err)
	}
	_ = cluster.Stop(dropID)
	cfg.logf("fault membership-swap: completed %v -> %v (new leader %d)", voters, nextVoters, newID)
	return nextVoters, nil
}

func waitNodeCaughtUp(ctx context.Context, cluster *Cluster, nodeID uint64) error {
	leaderID, _, err := cluster.WaitLeader(ctx, 0)
	if err != nil {
		return err
	}
	stLeader, err := cluster.Status(ctx, leaderID)
	if err != nil {
		return err
	}
	targetSeq := stLeader.GetAppliedSeq()
	ticker := time.NewTicker(50 * time.Millisecond)
	defer ticker.Stop()
	for {
		if st, err := cluster.Status(ctx, nodeID); err == nil && st.GetAppliedSeq() >= targetSeq {
			return nil
		}
		select {
		case <-ctx.Done():
			return fmt.Errorf("node %d did not reach seq %d: %w", nodeID, targetSeq, ctx.Err())
		case <-ticker.C:
		}
	}
}

func sleepCtx(ctx context.Context, d time.Duration) bool {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-t.C:
		return true
	case <-ctx.Done():
		return false
	}
}
