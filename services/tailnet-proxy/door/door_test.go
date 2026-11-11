package door

import (
	"encoding/json"
	"errors"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"testing"
)

// The way tailnet-proxy uses it: Tailscale headers, and a client that isn't
// a browser (curl, a script) calling the API.
func TestTailscaleStyleCaller(t *testing.T) {
	dir, err := os.MkdirTemp("", "door")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	socket := filepath.Join(dir, "a.sock")
	ln, err := net.Listen("unix", socket)
	if err != nil {
		t.Fatal(err)
	}
	seen := make(chan http.Header, 1)
	agentd := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		seen <- r.Header.Clone()
		_, _ = io.WriteString(w, "{}")
	})}
	go agentd.Serve(ln)
	t.Cleanup(func() { agentd.Close() })
	novnc, _ := url.Parse("http://127.0.0.1:1")

	allowed := true
	h := Handler(Config{
		Identify: func(r *http.Request) (Caller, error) {
			if !allowed {
				return Caller{Name: "stranger@github"}, errors.New("not on the allowlist")
			}
			return Caller{Name: "me@github", Headers: map[string]string{"X-Tailscale-User": "me@github", "X-Tailscale-Node": "my-mac"}}, nil
		},
		Agentd: socket, NoVNC: novnc,
	})
	srv := httptest.NewServer(h)
	t.Cleanup(srv.Close)

	// A script's POST: no Origin, no Sec-Fetch-Site. It gets through, and the
	// caller's own copy of the headers is replaced.
	req, _ := http.NewRequest("POST", srv.URL+"/api/v1/stop", nil)
	req.Header.Set("X-Tailscale-User", "admin@github")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != 200 {
		t.Fatalf("a script's POST: %d", resp.StatusCode)
	}
	got := <-seen
	if got.Get("X-Tailscale-User") != "me@github" || got.Get("X-Tailscale-Node") != "my-mac" {
		t.Errorf("agentd saw %v", got)
	}

	// A browser page on another site can't.
	req, _ = http.NewRequest("POST", srv.URL+"/api/v1/stop", nil)
	req.Header.Set("Sec-Fetch-Site", "cross-site")
	if resp, _ := http.DefaultClient.Do(req); resp.StatusCode != 403 {
		t.Errorf("a cross-site POST: %d", resp.StatusCode)
	}

	// Who's signed in; and no web app without -web.
	resp, _ = http.Get(srv.URL + "/session")
	var session map[string]string
	_ = json.NewDecoder(resp.Body).Decode(&session)
	if session["email"] != "me@github" {
		t.Errorf("/session = %v", session)
	}
	if resp, _ := http.Get(srv.URL + "/"); resp.StatusCode != 404 {
		t.Errorf("/ without a web app: %d", resp.StatusCode)
	}

	// Someone the way in refuses gets nothing.
	allowed = false
	if resp, _ := http.Get(srv.URL + "/session"); resp.StatusCode != 403 {
		t.Errorf("refused caller: %d", resp.StatusCode)
	}
}
