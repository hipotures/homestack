// SPDX-License-Identifier: AGPL-3.0-only
// Compiled into github.com/fosrl/cli/cmd/up/site, in the connector process.
package site

import (
	"context"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"os"
	"strconv"
	"sync"
	"time"

	newtpkg "github.com/fosrl/newt/newt"
)

type heartbeat struct {
	mu       sync.Mutex
	started  time.Time
	last     time.Time
	count    uint64
	interval time.Duration
}

// Newt already probes the server through its userspace WireGuard data path.
// Count only real successful probes, at most once per configured interval.
// A blocked tunnel worker cannot advance this heartbeat.
func (h *heartbeat) success() {
	h.mu.Lock()
	defer h.mu.Unlock()
	now := time.Now()
	if h.count == 0 || now.Sub(h.last) >= h.interval {
		h.last, h.count = now, h.count+1
	}
}

func (h *heartbeat) state() (uint64, uint64, bool) {
	h.mu.Lock()
	defer h.mu.Unlock()
	last := h.last
	if last.IsZero() {
		last = h.started
	}
	age := time.Since(last)
	return uint64(age / time.Second), h.count, age <= 3*h.interval
}

func startHomeStack(ctx context.Context, n *newtpkg.Newt) (func(), error) {
	seconds := 60
	if value := os.Getenv("HOMESTACK_HEARTBEAT_SECONDS"); value != "" {
		parsed, err := strconv.Atoi(value)
		if err != nil || parsed < 10 || parsed > 3600 {
			return nil, fmt.Errorf("invalid HOMESTACK_HEARTBEAT_SECONDS")
		}
		seconds = parsed
	}
	h := &heartbeat{started: time.Now(), interval: time.Duration(seconds) * time.Second}
	n.OnTunnelProbe = h.success
	telemetry := newTelemetry(h.started)
	mux := http.NewServeMux()
	health := func(ready bool) http.HandlerFunc {
		return func(w http.ResponseWriter, r *http.Request) {
			age, count, live := h.state()
			status, code := "pass", http.StatusOK
			if !live || (ready && count == 0) {
				status, code = "fail", http.StatusServiceUnavailable
			}
			beat := "pass"
			if !live {
				beat = "fail"
			}
			checks := map[string]any{
				"heartbeat:age":   []any{map[string]any{"status": beat, "observedValue": age, "observedUnit": "s"}},
				"heartbeat:count": []any{map[string]any{"status": beat, "observedValue": count}},
			}
			if ready {
				tunnel := "pass"
				if !live || count == 0 {
					tunnel = "fail"
				}
				checks["tunnel:connection"] = []any{map[string]any{"status": tunnel}}
			}
			w.Header().Set("Content-Type", "application/health+json")
			w.Header().Set("Cache-Control", "no-store")
			w.WriteHeader(code)
			_ = json.NewEncoder(w).Encode(map[string]any{"status": status, "checks": checks})
		}
	}
	mux.HandleFunc("GET /health/live", health(false))
	mux.HandleFunc("GET /health/ready", health(true))
	mux.HandleFunc("GET /.well-known/homestack/status", func(w http.ResponseWriter, r *http.Request) {
		_, _, live := h.state()
		status := "pass"
		if !live {
			status = "fail"
		}
		body, err := json.Marshal(telemetry.report(r.Context(), status))
		if err != nil || len(body) > 64*1024 {
			http.Error(w, "telemetry unavailable", http.StatusServiceUnavailable)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("Cache-Control", "no-store")
		_, _ = w.Write(body)
	})
	listener, err := net.Listen("tcp", ":8080")
	if err != nil {
		telemetry.close()
		return nil, fmt.Errorf("HomeStack management listener unavailable")
	}
	server := &http.Server{Handler: mux, ReadHeaderTimeout: 2 * time.Second, ReadTimeout: 3 * time.Second,
		WriteTimeout: 4 * time.Second, IdleTimeout: 10 * time.Second, MaxHeaderBytes: 8192}
	go func() {
		if err := server.Serve(listener); err != nil && err != http.ErrServerClosed {
			// runit restarts the entire connector if management can no longer serve.
			os.Exit(1)
		}
	}()
	return func() {
		_ = server.Close()
		telemetry.close()
	}, nil
}
