package lincheck

import (
	"context"
	"net"
	"sync"
	"testing"
	"time"

	"github.com/anishathalye/porcupine"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"

	kvv1 "peppydb/proto/kv/v1/go"
)

func TestModelValidSequentialAndConcurrentHistories(t *testing.T) {
	ops := []porcupine.Operation{
		// Initial absent read.
		{ClientId: 0, Input: Input{Kind: OpGet, Key: "k"}, Call: 1, Output: Output{Found: false}, Return: 2},
		// Put empty value (must be distinguished from absent).
		{ClientId: 0, Input: Input{Kind: OpPut, Key: "k", Value: ""}, Call: 3, Output: Output{}, Return: 4},
		{ClientId: 1, Input: Input{Kind: OpGet, Key: "k"}, Call: 5, Output: Output{Found: true, Value: ""}, Return: 6},
		// Concurrent puts v1 and v2; read during overlap sees v2, final read sees v1.
		{ClientId: 0, Input: Input{Kind: OpPut, Key: "k", Value: "v1"}, Call: 10, Output: Output{}, Return: 30},
		{ClientId: 1, Input: Input{Kind: OpPut, Key: "k", Value: "v2"}, Call: 12, Output: Output{}, Return: 20},
		{ClientId: 2, Input: Input{Kind: OpGet, Key: "k"}, Call: 15, Output: Output{Found: true, Value: "v2"}, Return: 25},
		{ClientId: 2, Input: Input{Kind: OpGet, Key: "k"}, Call: 31, Output: Output{Found: true, Value: "v1"}, Return: 35},
		// Delete followed by absent read.
		{ClientId: 0, Input: Input{Kind: OpDelete, Key: "k"}, Call: 40, Output: Output{}, Return: 45},
		{ClientId: 1, Input: Input{Kind: OpGet, Key: "k"}, Call: 46, Output: Output{Found: false}, Return: 50},
	}
	res, _ := Check(ops, 5*time.Second)
	if res != porcupine.Ok {
		t.Fatalf("Check = %v, want Ok", res)
	}
}

func TestModelUnknownOutcomeWrites(t *testing.T) {
	t.Run("unknown put that took effect", func(t *testing.T) {
		ops := []porcupine.Operation{
			{ClientId: 0, Input: Input{Kind: OpPut, Key: "k", Value: "v1"}, Call: 10, Output: Output{Unknown: true}, Return: 15},
			{ClientId: 1, Input: Input{Kind: OpGet, Key: "k"}, Call: 20, Output: Output{Found: true, Value: "v1"}, Return: 25},
		}
		if res, _ := Check(ops, 5*time.Second); res != porcupine.Ok {
			t.Fatalf("Check = %v, want Ok", res)
		}
	})

	t.Run("unknown put that did not take effect", func(t *testing.T) {
		ops := []porcupine.Operation{
			{ClientId: 0, Input: Input{Kind: OpPut, Key: "k", Value: "v0"}, Call: 1, Output: Output{}, Return: 5},
			{ClientId: 1, Input: Input{Kind: OpPut, Key: "k", Value: "v1"}, Call: 10, Output: Output{Unknown: true}, Return: 15},
			{ClientId: 2, Input: Input{Kind: OpGet, Key: "k"}, Call: 20, Output: Output{Found: true, Value: "v0"}, Return: 25},
		}
		if res, _ := Check(ops, 5*time.Second); res != porcupine.Ok {
			t.Fatalf("Check = %v, want Ok", res)
		}
	})

	t.Run("unknown delete that did not take effect", func(t *testing.T) {
		ops := []porcupine.Operation{
			{ClientId: 0, Input: Input{Kind: OpPut, Key: "k", Value: "v0"}, Call: 1, Output: Output{}, Return: 5},
			{ClientId: 1, Input: Input{Kind: OpDelete, Key: "k"}, Call: 10, Output: Output{Unknown: true}, Return: 15},
			{ClientId: 2, Input: Input{Kind: OpGet, Key: "k"}, Call: 20, Output: Output{Found: true, Value: "v0"}, Return: 25},
		}
		if res, _ := Check(ops, 5*time.Second); res != porcupine.Ok {
			t.Fatalf("Check = %v, want Ok", res)
		}
	})

	t.Run("unknown delete that took effect", func(t *testing.T) {
		ops := []porcupine.Operation{
			{ClientId: 0, Input: Input{Kind: OpPut, Key: "k", Value: "v0"}, Call: 1, Output: Output{}, Return: 5},
			{ClientId: 1, Input: Input{Kind: OpDelete, Key: "k"}, Call: 10, Output: Output{Unknown: true}, Return: 15},
			{ClientId: 2, Input: Input{Kind: OpGet, Key: "k"}, Call: 20, Output: Output{Found: false}, Return: 25},
		}
		if res, _ := Check(ops, 5*time.Second); res != porcupine.Ok {
			t.Fatalf("Check = %v, want Ok", res)
		}
	})
}

func TestModelRejectsNonLinearizableHistories(t *testing.T) {
	t.Run("stale read after completed put", func(t *testing.T) {
		ops := []porcupine.Operation{
			{ClientId: 0, Input: Input{Kind: OpPut, Key: "k", Value: "v1"}, Call: 1, Output: Output{}, Return: 5},
			{ClientId: 0, Input: Input{Kind: OpPut, Key: "k", Value: "v2"}, Call: 6, Output: Output{}, Return: 10},
			{ClientId: 1, Input: Input{Kind: OpGet, Key: "k"}, Call: 11, Output: Output{Found: true, Value: "v1"}, Return: 15},
		}
		if res, _ := Check(ops, 5*time.Second); res != porcupine.Illegal {
			t.Fatalf("Check = %v, want Illegal", res)
		}
	})

	t.Run("lost acknowledged write", func(t *testing.T) {
		ops := []porcupine.Operation{
			{ClientId: 0, Input: Input{Kind: OpPut, Key: "k", Value: "v1"}, Call: 1, Output: Output{}, Return: 5},
			{ClientId: 1, Input: Input{Kind: OpGet, Key: "k"}, Call: 6, Output: Output{Found: false}, Return: 10},
		}
		if res, _ := Check(ops, 5*time.Second); res != porcupine.Illegal {
			t.Fatalf("Check = %v, want Illegal", res)
		}
	})

	t.Run("stale read after completed delete", func(t *testing.T) {
		ops := []porcupine.Operation{
			{ClientId: 0, Input: Input{Kind: OpPut, Key: "k", Value: "v1"}, Call: 1, Output: Output{}, Return: 5},
			{ClientId: 0, Input: Input{Kind: OpDelete, Key: "k"}, Call: 6, Output: Output{}, Return: 10},
			{ClientId: 1, Input: Input{Kind: OpGet, Key: "k"}, Call: 11, Output: Output{Found: true, Value: "v1"}, Return: 15},
		}
		if res, _ := Check(ops, 5*time.Second); res != porcupine.Illegal {
			t.Fatalf("Check = %v, want Illegal", res)
		}
	})

	t.Run("non-linearizable register flip-flop", func(t *testing.T) {
		// Put(v1) and Put(v2) both overlap, followed by strictly ordered reads
		// seeing v1, then v2, then v1 again — impossible for one linear order.
		ops := []porcupine.Operation{
			{ClientId: 0, Input: Input{Kind: OpPut, Key: "k", Value: "v1"}, Call: 1, Output: Output{}, Return: 10},
			{ClientId: 1, Input: Input{Kind: OpPut, Key: "k", Value: "v2"}, Call: 1, Output: Output{}, Return: 10},
			{ClientId: 2, Input: Input{Kind: OpGet, Key: "k"}, Call: 2, Output: Output{Found: true, Value: "v1"}, Return: 3},
			{ClientId: 2, Input: Input{Kind: OpGet, Key: "k"}, Call: 4, Output: Output{Found: true, Value: "v2"}, Return: 5},
			{ClientId: 2, Input: Input{Kind: OpGet, Key: "k"}, Call: 11, Output: Output{Found: true, Value: "v1"}, Return: 12},
		}
		if res, _ := Check(ops, 5*time.Second); res != porcupine.Illegal {
			t.Fatalf("Check = %v, want Illegal", res)
		}
	})
}

type memKV struct {
	kvv1.UnimplementedKvServiceServer
	mu   sync.Mutex
	data map[string][]byte
}

func (m *memKV) Get(_ context.Context, req *kvv1.GetRequest) (*kvv1.GetResponse, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	v, ok := m.data[string(req.Key)]
	if !ok {
		return &kvv1.GetResponse{}, nil
	}
	return &kvv1.GetResponse{Value: append([]byte(nil), v...)}, nil
}

func (m *memKV) Put(_ context.Context, req *kvv1.PutRequest) (*kvv1.PutResponse, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.data[string(req.Key)] = append([]byte(nil), req.Value...)
	return &kvv1.PutResponse{}, nil
}

func (m *memKV) Delete(_ context.Context, req *kvv1.DeleteRequest) (*kvv1.DeleteResponse, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	delete(m.data, string(req.Key))
	return &kvv1.DeleteResponse{}, nil
}

func TestRunWorkloadAgainstInMemoryServer(t *testing.T) {
	lis, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	srv := grpc.NewServer()
	kvv1.RegisterKvServiceServer(srv, &memKV{data: make(map[string][]byte)})
	go func() {
		_ = srv.Serve(lis)
	}()
	t.Cleanup(srv.Stop)

	conn, err := grpc.NewClient(lis.Addr().String(), grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	t.Cleanup(func() { _ = conn.Close() })

	rec, err := RunWorkload(context.Background(), kvv1.NewKvServiceClient(conn), WorkloadConfig{
		Workers:  4,
		Keys:     3,
		Duration: 150 * time.Millisecond,
	})
	if err != nil {
		t.Fatalf("RunWorkload: %v", err)
	}
	st := rec.Stats()
	if st.Gets+st.Puts+st.Deletes == 0 {
		t.Fatalf("expected recorded operations, got %+v", st)
	}
	res, _ := Check(rec.Operations(), 5*time.Second)
	if res != porcupine.Ok {
		t.Fatalf("Porcupine check = %v, want Ok (stats=%+v)", res, st)
	}
}
