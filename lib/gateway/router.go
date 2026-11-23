// Package gateway implements the stateless peppydb gRPC gateway (plan 0033).
//
// It serves peppydb.kv.v1.KvService and grpc.health.v1.Health over a Router
// seam (a single static Raft group in phase 2; placement-backed μshard routing
// in phase 3), with optional Prometheus metrics over HTTP.
package gateway

import (
	"context"
	"errors"

	kvclient "peppydb/lib/gateway-kvclient"
	kvv1 "peppydb/proto/kv/v1/go"
)

// Backend executes KV RPCs against one Raft group, tracking its leader.
// *kvclient.Client implements Backend.
type Backend interface {
	Get(ctx context.Context, req *kvv1.GetRequest) (*kvv1.GetResponse, error)
	Put(ctx context.Context, req *kvv1.PutRequest) (*kvv1.PutResponse, error)
	Delete(ctx context.Context, req *kvv1.DeleteRequest) (*kvv1.DeleteResponse, error)
	Scan(ctx context.Context, req *kvv1.ScanRequest) (*kvv1.ScanResponse, error)
	Close() error
}

// Router resolves a key or scan to a backend Raft group.
// Phase 2 uses StaticRouter (one group); phase 3 plugs in a placement router.
type Router interface {
	ClientForKey(key []byte) (Backend, error)
	DefaultClient() (Backend, error)
	Close() error
}

// StaticRouter routes every key and scan to a single Backend (phase 2).
type StaticRouter struct {
	backend Backend
}

// NewStaticRouter creates a StaticRouter backed by a leader-tracking
// kvclient.Client connected to seeds.
func NewStaticRouter(seeds []string, cfg Config, metrics *Metrics) (*StaticRouter, error) {
	opts := kvclient.Options{
		Seeds:          seeds,
		MaxRedirects:   cfg.MaxRedirects,
		MaxReadRetries: cfg.MaxReadRetries,
		RetryBackoff:   cfg.RetryBackoff,
	}
	if metrics != nil {
		opts.Hooks = kvclient.Hooks{
			OnRedirect:  metrics.IncLeaderRedirect,
			OnReadRetry: metrics.IncReadRetry,
			OnProbe:     metrics.IncLeaderProbe,
		}
	}
	client, err := kvclient.New(opts)
	if err != nil {
		return nil, err
	}
	return &StaticRouter{backend: client}, nil
}

// NewStaticRouterWithBackend wraps an existing Backend (useful in tests).
func NewStaticRouterWithBackend(backend Backend) (*StaticRouter, error) {
	if backend == nil {
		return nil, errors.New("gateway: backend must not be nil")
	}
	return &StaticRouter{backend: backend}, nil
}

// ClientForKey returns the single configured backend.
func (r *StaticRouter) ClientForKey(_ []byte) (Backend, error) {
	return r.backend, nil
}

// DefaultClient returns the single configured backend.
func (r *StaticRouter) DefaultClient() (Backend, error) {
	return r.backend, nil
}

// Close closes the underlying backend client.
func (r *StaticRouter) Close() error {
	return r.backend.Close()
}
