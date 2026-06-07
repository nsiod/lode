// Package main is a lode demo app (Go). See ../README.md.
//
// It conforms to the language-agnostic lode app contract and shows the three
// things an app does under lode:
//
//	1. START   — bind $PORT and serve; lode runs this binary as its child.
//	2. READ    — read the env lode injects (LODE_ACTIVE_VERSION / LODE_DATA_DIR /
//	             LODE_INSTANCE) plus passthrough host env (PORT, operator [env]).
//	3. UPGRADE — participate in updates two ways:
//	     a) PASSIVE: announce readiness (state.ready = LODE_INSTANCE) and handle
//	        SIGTERM gracefully, so lode's update/rollback is seamless;
//	     b) ACTIVE:  POST /upgrade writes state.target = "latest" to ASK lode to
//	        pull the newest version; POST /restart bumps state.restart_nonce.
//
// Standalone (no lode): LODE_DATA_DIR is unset, so the state.json steps are
// no-ops and you still get a working server for `start` + `read`.
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"
)

// buildVersion is the fallback baked at build time:
//
//	go build -ldflags "-X main.buildVersion=1.2.3" -o demo-go .
//
// At runtime lode's LODE_ACTIVE_VERSION wins, so /version always matches what
// lode actually installed.
var buildVersion = "0.0.0-dev"

func version() string {
	if v := os.Getenv("LODE_ACTIVE_VERSION"); v != "" {
		return v
	}
	return buildVersion
}

func env(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

func log(format string, a ...any) { fmt.Printf("[demo-go] "+format+"\n", a...) }

func main() {
	// `lode version` passthrough when the operator sets exec = "{entry}".
	if len(os.Args) > 1 {
		switch os.Args[1] {
		case "version", "--version", "-v":
			fmt.Println(version())
			return
		}
	}

	port := env("PORT", "8080")
	addr := ":" + port

	mux := http.NewServeMux()
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, _ *http.Request) {
		fmt.Fprintln(w, "ok")
	})
	mux.HandleFunc("/version", func(w http.ResponseWriter, _ *http.Request) {
		fmt.Fprintln(w, version())
	})
	// READ: surface the env lode injected + passthrough host/operator env.
	mux.HandleFunc("/env", func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]any{
			"version":  version(),                  // LODE_ACTIVE_VERSION or baked
			"instance": os.Getenv("LODE_INSTANCE"), // unique id per launch
			"dataDir":  os.Getenv("LODE_DATA_DIR"), // where state.json lives
			"port":     port,                       // host env passthrough
			"greeting": os.Getenv("APP_GREETING"),  // operator [env] / host -e
		})
	})
	// UPGRADE (active): ask lode to pull the newest version.
	mux.HandleFunc("/upgrade", func(w http.ResponseWriter, _ *http.Request) {
		if err := patchState(map[string]any{"target": "latest"}); err != nil {
			http.Error(w, err.Error(), http.StatusServiceUnavailable)
			return
		}
		fmt.Fprintln(w, "requested update to latest")
	})
	// UPGRADE (active): ask lode to restart the current version.
	mux.HandleFunc("/restart", func(w http.ResponseWriter, _ *http.Request) {
		if err := bumpRestart(); err != nil {
			http.Error(w, err.Error(), http.StatusServiceUnavailable)
			return
		}
		fmt.Fprintln(w, "requested restart")
	})

	srv := &http.Server{Handler: mux}

	// START: bind first so readiness is announced only once we can serve.
	ln, err := net.Listen("tcp", addr)
	if err != nil {
		fmt.Fprintf(os.Stderr, "[demo-go] bind %s: %v\n", addr, err)
		os.Exit(1)
	}
	log("starting version=%s pid=%d instance=%s data_dir=%s addr=%s",
		version(), os.Getpid(), env("LODE_INSTANCE", "none"), env("LODE_DATA_DIR", "unset"), addr)

	// UPGRADE (passive): graceful stop. On SIGTERM/SIGINT drain and exit(0) well
	// within supervise.stop_timeout, or lode SIGKILLs us.
	stop := make(chan os.Signal, 1)
	signal.Notify(stop, syscall.SIGTERM, syscall.SIGINT)
	go func() {
		s := <-stop
		log("%v received — shutting down", s)
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = srv.Shutdown(ctx)
	}()

	// UPGRADE (passive): announce readiness so lode (readiness="state") commits us.
	announceReady()

	if err := srv.Serve(ln); err != nil && err != http.ErrServerClosed {
		fmt.Fprintf(os.Stderr, "[demo-go] serve: %v\n", err)
		os.Exit(1)
	}
	log("cleanup done, exiting 0")
}

// --- state.json: the app <-> lode comms file under $LODE_DATA_DIR -----------

func statePath() (string, bool) {
	dir := os.Getenv("LODE_DATA_DIR")
	if dir == "" {
		return "", false // standalone (not under lode)
	}
	return filepath.Join(dir, "state.json"), true
}

// patchState merges fields into state.json (atomic temp+rename), preserving
// lode's own fields so it never reads a half-written file.
func patchState(fields map[string]any) error {
	path, ok := statePath()
	if !ok {
		return fmt.Errorf("not running under lode (LODE_DATA_DIR unset)")
	}
	state := map[string]any{}
	if b, err := os.ReadFile(path); err == nil {
		_ = json.Unmarshal(b, &state) // tolerate empty/corrupt: start fresh
	}
	for k, v := range fields {
		state[k] = v
	}
	b, err := json.MarshalIndent(state, "", "  ")
	if err != nil {
		return err
	}
	b = append(b, '\n')
	tmp := fmt.Sprintf("%s.tmp.%d", path, os.Getpid())
	if err := os.WriteFile(tmp, b, 0o644); err != nil {
		return err
	}
	if err := os.Rename(tmp, path); err != nil {
		_ = os.Remove(tmp)
		return err
	}
	return nil
}

func bumpRestart() error {
	path, ok := statePath()
	if !ok {
		return fmt.Errorf("not running under lode (LODE_DATA_DIR unset)")
	}
	n := 0.0
	if b, err := os.ReadFile(path); err == nil {
		var s map[string]any
		if json.Unmarshal(b, &s) == nil {
			if v, ok := s["restart_nonce"].(float64); ok { // JSON numbers decode to float64
				n = v
			}
		}
	}
	return patchState(map[string]any{"restart_nonce": n + 1})
}

// announceReady writes state.ready = $LODE_INSTANCE so lode (readiness="state")
// marks this version running/good. No-op standalone.
func announceReady() {
	inst := os.Getenv("LODE_INSTANCE")
	if err := patchState(map[string]any{"ready": inst}); err != nil {
		log("readiness skipped: %v", err)
		return
	}
	path, _ := statePath()
	log("ready: wrote state.ready=%s -> %s", inst, path)
}
