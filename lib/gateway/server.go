package gateway

import (
	"context"
	"errors"
	"net"
	"net/http"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/health"
	"google.golang.org/grpc/health/grpc_health_v1"
	"google.golang.org/grpc/status"

	kvv1 "peppydb/proto/kv/v1/go"
)

const (
	defaultRequestTimeout  = 5 * time.Second
	metricsShutdownTimeout = 2 * time.Second
	kvServiceName          = "peppydb.kv.v1.KvService"
)

// Config tunes gateway request timeouts and backend retry limits.
type Config struct {
	// RequestTimeout is applied when an incoming RPC has no deadline. Defaults to 5s.
	RequestTimeout time.Duration
	// MaxRedirects caps LeaderHint redirects per RPC.
	MaxRedirects int
	// MaxReadRetries caps non-hint UNAVAILABLE retries for reads.
	MaxReadRetries int
	// RetryBackoff is the initial backoff between read retries.
	RetryBackoff time.Duration
}

// Server implements peppydb.kv.v1.KvService over a Router.
type Server struct {
	kvv1.UnimplementedKvServiceServer
	router  Router
	cfg     Config
	metrics *Metrics
}

// NewServer constructs a KvServiceServer over router.
func NewServer(router Router, cfg Config, metrics *Metrics) (*Server, error) {
	if router == nil {
		return nil, errors.New("gateway: router must not be nil")
	}
	if cfg.RequestTimeout <= 0 {
		cfg.RequestTimeout = defaultRequestTimeout
	}
	return &Server{
		router:  router,
		cfg:     cfg,
		metrics: metrics,
	}, nil
}

func (s *Server) withTimeout(ctx context.Context) (context.Context, context.CancelFunc) {
	if _, hasDeadline := ctx.Deadline(); hasDeadline {
		return ctx, func() {}
	}
	return context.WithTimeout(ctx, s.cfg.RequestTimeout)
}

// Get routes a single-key Get to the backend for req.Key.
func (s *Server) Get(ctx context.Context, req *kvv1.GetRequest) (*kvv1.GetResponse, error) {
	return observeCall(s, "get", ctx, func(ctx context.Context) (*kvv1.GetResponse, error) {
		backend, err := s.router.ClientForKey(req.GetKey())
		if err != nil {
			return nil, routingError(err)
		}
		return backend.Get(ctx, req)
	})
}

// Put routes a single-key Put to the backend for req.Key.
func (s *Server) Put(ctx context.Context, req *kvv1.PutRequest) (*kvv1.PutResponse, error) {
	return observeCall(s, "put", ctx, func(ctx context.Context) (*kvv1.PutResponse, error) {
		backend, err := s.router.ClientForKey(req.GetKey())
		if err != nil {
			return nil, routingError(err)
		}
		return backend.Put(ctx, req)
	})
}

// Delete routes a single-key Delete to the backend for req.Key.
func (s *Server) Delete(ctx context.Context, req *kvv1.DeleteRequest) (*kvv1.DeleteResponse, error) {
	return observeCall(s, "delete", ctx, func(ctx context.Context) (*kvv1.DeleteResponse, error) {
		backend, err := s.router.ClientForKey(req.GetKey())
		if err != nil {
			return nil, routingError(err)
		}
		return backend.Delete(ctx, req)
	})
}

// Scan routes a range Scan to the default backend (single shard in phase 2).
func (s *Server) Scan(ctx context.Context, req *kvv1.ScanRequest) (*kvv1.ScanResponse, error) {
	return observeCall(s, "scan", ctx, func(ctx context.Context) (*kvv1.ScanResponse, error) {
		backend, err := s.router.DefaultClient()
		if err != nil {
			return nil, routingError(err)
		}
		return backend.Scan(ctx, req)
	})
}

func observeCall[Resp any](
	s *Server,
	method string,
	ctx context.Context,
	fn func(context.Context) (Resp, error),
) (Resp, error) {
	s.metrics.incInflight()
	defer s.metrics.decInflight()

	began := time.Now()
	callCtx, cancel := s.withTimeout(ctx)
	defer cancel()

	resp, err := fn(callCtx)
	s.metrics.observeRPC(method, err, time.Since(began))
	return resp, err
}

func routingError(err error) error {
	if _, ok := status.FromError(err); ok {
		return err
	}
	return status.Errorf(codes.Unavailable, "route request: %v", err)
}

// Serve runs the gRPC gateway on lis (and optional HTTP metrics on metricsLis)
// until ctx is cancelled, then drains gracefully and closes router.
func Serve(
	ctx context.Context,
	lis net.Listener,
	metricsLis net.Listener,
	router Router,
	cfg Config,
	metrics *Metrics,
) error {
	srv, err := NewServer(router, cfg, metrics)
	if err != nil {
		return err
	}
	grpcServer := grpc.NewServer()
	kvv1.RegisterKvServiceServer(grpcServer, srv)

	healthSrv := health.NewServer()
	healthSrv.SetServingStatus("", grpc_health_v1.HealthCheckResponse_SERVING)
	healthSrv.SetServingStatus(kvServiceName, grpc_health_v1.HealthCheckResponse_SERVING)
	grpc_health_v1.RegisterHealthServer(grpcServer, healthSrv)

	var httpSrv *http.Server
	httpErrCh := make(chan error, 1)
	if metricsLis != nil && metrics != nil {
		httpSrv = &http.Server{
			Handler:           metrics.Handler(),
			ReadHeaderTimeout: 5 * time.Second,
		}
		go func() {
			err := httpSrv.Serve(metricsLis)
			if errors.Is(err, http.ErrServerClosed) {
				err = nil
			}
			httpErrCh <- err
		}()
	}

	grpcErrCh := make(chan error, 1)
	go func() {
		grpcErrCh <- grpcServer.Serve(lis)
	}()

	var serveErr error
	select {
	case <-ctx.Done():
		healthSrv.Shutdown()
		if httpSrv != nil {
			shutCtx, cancel := context.WithTimeout(context.Background(), metricsShutdownTimeout)
			_ = httpSrv.Shutdown(shutCtx)
			cancel()
		}
		grpcServer.GracefulStop()
		serveErr = <-grpcErrCh
	case err := <-grpcErrCh:
		serveErr = err
		healthSrv.Shutdown()
		if httpSrv != nil {
			shutCtx, cancel := context.WithTimeout(context.Background(), metricsShutdownTimeout)
			_ = httpSrv.Shutdown(shutCtx)
			cancel()
		}
	case err := <-httpErrCh:
		serveErr = err
		healthSrv.Shutdown()
		grpcServer.GracefulStop()
		<-grpcErrCh
	}

	_ = router.Close()
	if errors.Is(serveErr, grpc.ErrServerStopped) {
		return nil
	}
	return serveErr
}
