package main

import (
	"context"
	"crypto/rand"
	"crypto/rsa"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/go-jose/go-jose/v4"
)

// A stand-in for Cloudflare Access: a team whose certs endpoint serves our
// test key, and a way to mint tokens with any claims and any key.
type team struct {
	server *httptest.Server
	key    *rsa.PrivateKey
}

func newTeam(t *testing.T) *team {
	t.Helper()
	key, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		t.Fatal(err)
	}
	tm := &team{key: key}
	tm.server = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/cdn-cgi/access/certs" {
			http.NotFound(w, r)
			return
		}
		set := jose.JSONWebKeySet{Keys: []jose.JSONWebKey{{Key: &key.PublicKey, KeyID: "k1", Algorithm: "RS256", Use: "sig"}}}
		_ = json.NewEncoder(w).Encode(set)
	}))
	t.Cleanup(tm.server.Close)
	return tm
}

func (tm *team) token(t *testing.T, key *rsa.PrivateKey, claims map[string]any) string {
	t.Helper()
	signer, err := jose.NewSigner(jose.SigningKey{Algorithm: jose.RS256, Key: jose.JSONWebKey{Key: key, KeyID: "k1"}},
		(&jose.SignerOptions{}).WithType("JWT"))
	if err != nil {
		t.Fatal(err)
	}
	payload, _ := json.Marshal(claims)
	signed, err := signer.Sign(payload)
	if err != nil {
		t.Fatal(err)
	}
	compact, err := signed.CompactSerialize()
	if err != nil {
		t.Fatal(err)
	}
	return compact
}

func (tm *team) claims(email string) map[string]any {
	now := time.Now()
	return map[string]any{
		"iss":   tm.server.URL,
		"aud":   []string{"app-aud"},
		"email": email,
		"iat":   now.Unix(),
		"exp":   now.Add(time.Hour).Unix(),
		"sub":   "user-1",
	}
}

// What agentd and the viewer saw.
type seen struct {
	path, user, jwt, cookie string
}

func backends(t *testing.T) (socket string, novnc *url.URL, got chan seen) {
	t.Helper()
	got = make(chan seen, 1)
	record := func(w http.ResponseWriter, r *http.Request) {
		got <- seen{r.URL.Path, r.Header.Get("X-Agent-User"), r.Header.Get("Cf-Access-Jwt-Assertion"), r.Header.Get("Cookie")}
		_, _ = io.WriteString(w, "ok")
	}
	dir, err := os.MkdirTemp("", "fd")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	socket = filepath.Join(dir, "agentd.sock")
	ln, err := net.Listen("unix", socket)
	if err != nil {
		t.Fatal(err)
	}
	agentd := &http.Server{Handler: http.HandlerFunc(record)}
	go agentd.Serve(ln)
	t.Cleanup(func() { agentd.Close() })
	viewer := httptest.NewServer(http.HandlerFunc(record))
	t.Cleanup(viewer.Close)
	novnc, _ = url.Parse(viewer.URL)
	return socket, novnc, got
}

func TestOnlyValidAccessTokensGetIn(t *testing.T) {
	tm := newTeam(t)
	socket, novnc, got := backends(t)
	g := newGate(context.Background(), Config{TeamDomain: tm.server.URL, AUD: "app-aud", Allow: []string{"@acme.com", "Contractor@Example.com"}})
	front := httptest.NewServer(handler(g, novnc, socket, ""))
	t.Cleanup(front.Close)

	otherKey, _ := rsa.GenerateKey(rand.Reader, 2048)
	expired := tm.claims("me@acme.com")
	expired["exp"] = time.Now().Add(-time.Minute).Unix()
	wrongAud := tm.claims("me@acme.com")
	wrongAud["aud"] = []string{"another-app"}
	wrongIss := tm.claims("me@acme.com")
	wrongIss["iss"] = "https://evil.cloudflareaccess.com"
	service := tm.claims("")
	delete(service, "email")
	service["common_name"] = "abc.access"

	cases := []struct {
		name   string
		token  string
		path   string
		status int
		user   string
		// The path the backend sees, when it isn't path.
		backend string
	}{
		{"a person at the customer's domain", tm.token(t, tm.key, tm.claims("me@acme.com")), "/api/v1/events", 200, "me@acme.com", ""},
		{"a listed email, any case", tm.token(t, tm.key, tm.claims("contractor@example.com")), "/api/v1/events", 200, "contractor@example.com", ""},
		{"the viewer", tm.token(t, tm.key, tm.claims("me@acme.com")), "/viewer/vnc.html", 200, "", "/vnc.html"},
		{"the viewer's stream", tm.token(t, tm.key, tm.claims("me@acme.com")), "/websockify", 200, "", ""},
		{"no token", "", "/api/v1/events", 403, "", ""},
		{"not a token", "nonsense", "/api/v1/events", 403, "", ""},
		{"signed by someone else", tm.token(t, otherKey, tm.claims("me@acme.com")), "/api/v1/events", 403, "", ""},
		{"expired", tm.token(t, tm.key, expired), "/api/v1/events", 403, "", ""},
		{"for another Access app", tm.token(t, tm.key, wrongAud), "/api/v1/events", 403, "", ""},
		{"from another team", tm.token(t, tm.key, wrongIss), "/api/v1/events", 403, "", ""},
		{"a service token", tm.token(t, tm.key, service), "/api/v1/events", 403, "", ""},
		{"not on the allow list", tm.token(t, tm.key, tm.claims("someone@gmail.com")), "/api/v1/events", 403, "", ""},
		{"a look-alike domain", tm.token(t, tm.key, tm.claims("me@notacme.com")), "/api/v1/events", 403, "", ""},
	}
	for _, c := range cases {
		req, _ := http.NewRequest("GET", front.URL+c.path, nil)
		if c.token != "" {
			req.Header.Set("Cf-Access-Jwt-Assertion", c.token)
		}
		// A caller can't name themselves, and Cloudflare's cookie stays here.
		req.Header.Set("X-Agent-User", "admin@acme.com")
		req.Header.Set("X-Tailscale-User", "admin@acme.com")
		req.Header.Set("Cookie", "CF_Authorization=secret")
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatalf("%s: %v", c.name, err)
		}
		resp.Body.Close()
		if resp.StatusCode != c.status {
			t.Errorf("%s: status %d, want %d", c.name, resp.StatusCode, c.status)
			continue
		}
		if c.status != 200 {
			continue
		}
		s := <-got
		want := c.path
		if c.backend != "" {
			want = c.backend
		}
		if s.path != want || s.user != c.user || s.jwt != "" || s.cookie != "" {
			t.Errorf("%s: backend saw %+v", c.name, s)
		}
	}
}

func TestSettings(t *testing.T) {
	if got := issuer(" acme.cloudflareaccess.com/ "); got != "https://acme.cloudflareaccess.com" {
		t.Errorf("issuer = %q", got)
	}
	dir := t.TempDir()
	path := filepath.Join(dir, "access.json")
	os.WriteFile(path, []byte(`{"team_domain":"acme.cloudflareaccess.com"}`), 0o600)
	if _, err := loadConfig(path); err == nil {
		t.Error("a config without aud loaded")
	}
	os.WriteFile(path, []byte(`{"team_domain":"acme.cloudflareaccess.com","aud":"x","allow":["@acme.com"]}`), 0o600)
	if cfg, err := loadConfig(path); err != nil || cfg.AUD != "x" || len(cfg.Allow) != 1 {
		t.Errorf("loadConfig = %+v, %v", cfg, err)
	}
	if _, err := listen("0.0.0.0:8080"); err == nil {
		t.Error("listened beyond localhost")
	}
	if ln, err := listen("127.0.0.1:0"); err != nil {
		t.Error(err)
	} else {
		ln.Close()
	}
	sock := filepath.Join(dir, "front.sock")
	if ln, err := listen("unix:" + sock); err != nil {
		t.Error(err)
	} else {
		info, _ := os.Stat(sock)
		if info.Mode().Perm() != 0o660 {
			t.Errorf("socket mode %v", info.Mode().Perm())
		}
		ln.Close()
	}
}

func TestWebAppSessionAndOtherSites(t *testing.T) {
	tm := newTeam(t)
	socket, novnc, got := backends(t)
	web := t.TempDir()
	os.MkdirAll(filepath.Join(web, "assets"), 0o755)
	os.WriteFile(filepath.Join(web, "index.html"), []byte("<!doctype html><title>app</title>"), 0o644)
	os.WriteFile(filepath.Join(web, "assets", "app.css"), []byte("body{}"), 0o644)
	g := newGate(context.Background(), Config{TeamDomain: tm.server.URL, AUD: "app-aud"})
	front := httptest.NewServer(handler(g, novnc, socket, web))
	t.Cleanup(front.Close)
	token := tm.token(t, tm.key, tm.claims("me@acme.com"))
	host := front.Listener.Addr().String()

	do := func(method, path string, headers map[string]string) *http.Response {
		t.Helper()
		req, _ := http.NewRequest(method, front.URL+path, nil)
		req.Header.Set("Cf-Access-Jwt-Assertion", token)
		for k, v := range headers {
			req.Header.Set(k, v)
		}
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() { resp.Body.Close() })
		return resp
	}

	// The page and its files, with the policy; nothing else of the folder.
	page := do("GET", "/", nil)
	body, _ := io.ReadAll(page.Body)
	if page.StatusCode != 200 || !strings.Contains(string(body), "<title>app</title>") ||
		!strings.Contains(page.Header.Get("Content-Security-Policy"), "frame-ancestors 'none'") {
		t.Errorf("/: %d %q %q", page.StatusCode, body, page.Header.Get("Content-Security-Policy"))
	}
	if r := do("GET", "/assets/app.css", nil); r.StatusCode != 200 {
		t.Errorf("/assets/app.css: %d", r.StatusCode)
	}
	for _, path := range []string{"/assets/", "/index.html.bak", "/vnc.html", "/../main.go"} {
		if r := do("GET", path, nil); r.StatusCode != 404 {
			t.Errorf("%s: %d, want 404", path, r.StatusCode)
		}
	}
	if r := do("GET", "/", nil); r.Header.Get("X-Content-Type-Options") != "nosniff" {
		t.Error("no nosniff")
	}

	// Who's signed in.
	var session map[string]string
	_ = json.NewDecoder(do("GET", "/session", nil).Body).Decode(&session)
	if session["email"] != "me@acme.com" || session["sign_out"] != "/cdn-cgi/access/logout" {
		t.Errorf("/session = %v", session)
	}

	// Changes only from the app's own pages.
	if r := do("POST", "/api/v1/stop", map[string]string{"Sec-Fetch-Site": "cross-site"}); r.StatusCode != 403 {
		t.Errorf("a cross-site POST: %d, want 403", r.StatusCode)
	}
	if r := do("POST", "/api/v1/stop", map[string]string{"Origin": "https://evil.example"}); r.StatusCode != 403 {
		t.Errorf("a POST from another origin: %d, want 403", r.StatusCode)
	}
	if r := do("POST", "/api/v1/stop", map[string]string{"Sec-Fetch-Site": "same-origin"}); r.StatusCode != 200 {
		t.Errorf("the app's own POST: %d", r.StatusCode)
	} else if s := <-got; s.path != "/api/v1/stop" {
		t.Errorf("agentd saw %+v", s)
	}

	// The viewer's WebSocket only from the app's pages.
	upgrade := map[string]string{"Upgrade": "websocket", "Connection": "Upgrade"}
	foreign := map[string]string{"Origin": "https://evil.example"}
	for k, v := range upgrade {
		foreign[k] = v
	}
	if r := do("GET", "/websockify", foreign); r.StatusCode != 403 {
		t.Errorf("a WebSocket from another site: %d, want 403", r.StatusCode)
	}
	own := map[string]string{"Origin": "http://" + host}
	for k, v := range upgrade {
		own[k] = v
	}
	if r := do("GET", "/websockify", own); r.StatusCode == 403 {
		t.Error("the app's own WebSocket was refused")
	} else {
		<-got
	}
	if r := do("GET", "/viewer/vnc.html", nil); r.Header.Get("X-Frame-Options") != "SAMEORIGIN" {
		t.Error("the viewer can be framed by other sites")
	} else {
		<-got
	}
}
