// front-door is a customer deployment's way in to the desktop
// (docs/customer-cloud.md). Cloudflare Access signs the person in through the
// customer's identity provider; Cloudflare Tunnel (cloudflared, on this VM,
// outbound only) brings the request here.
//
// Nothing gets past it without a valid Access token (Cf-Access-Jwt-Assertion):
// signed by the customer's Cloudflare team, for this application (its AUD
// tag), not expired, for a person (an email), and on the -allow list if there
// is one. A wrong Access setting in Cloudflare (the hostname left unprotected,
// say) then fails closed. Then it serves what package door does (the web app,
// agentd's API with the email in X-Agent-User, the viewer), as tailnet-proxy
// does on our desktops.
//
// It listens on localhost or a Unix socket only; the VM has no inbound port.
// Its settings (team domain, AUD tag, allow list) come from -config, a JSON
// file the unit fetches from SSM at each start, so the customer can change
// them without us.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"log"
	"net"
	"net/http"
	"net/url"
	"os"
	"slices"
	"strings"
	"time"

	"github.com/coreos/go-oidc/v3/oidc"

	"cloud-desktops/front-door/door"
)

// Config is the -config file.
type Config struct {
	// The customer's Cloudflare team domain: "acme.cloudflareaccess.com" (or
	// the full https URL). The token's issuer, and where its signing keys are.
	TeamDomain string `json:"team_domain"`
	// The Access application's AUD tag.
	AUD string `json:"aud"`
	// Emails ("me@acme.com") and whole domains ("@acme.com") let in. Empty:
	// whoever the Access policy lets through.
	Allow []string `json:"allow"`
}

func loadConfig(path string) (Config, error) {
	var cfg Config
	data, err := os.ReadFile(path)
	if err != nil {
		return cfg, err
	}
	if err := json.Unmarshal(data, &cfg); err != nil {
		return cfg, fmt.Errorf("%s: %w", path, err)
	}
	if cfg.TeamDomain == "" || cfg.AUD == "" {
		return cfg, fmt.Errorf("%s: team_domain and aud are required", path)
	}
	return cfg, nil
}

// issuer is the team's URL, which Access tokens name as their issuer.
func issuer(teamDomain string) string {
	team := strings.TrimRight(strings.TrimSpace(teamDomain), "/")
	if !strings.Contains(team, "://") {
		team = "https://" + team
	}
	return team
}

type gate struct {
	verifier *oidc.IDTokenVerifier
	allow    []string
}

func newGate(ctx context.Context, cfg Config) gate {
	iss := issuer(cfg.TeamDomain)
	keys := oidc.NewRemoteKeySet(ctx, iss+"/cdn-cgi/access/certs")
	return gate{
		verifier: oidc.NewVerifier(iss, keys, &oidc.Config{ClientID: cfg.AUD}),
		allow:    cfg.Allow,
	}
}

var (
	errNoToken   = errors.New("no Access token")
	errNoEmail   = errors.New("not a person (a service token?)")
	errNotListed = errors.New("not on the allow list")
)

// identity checks the request's Access token and returns who it's for.
func (g gate) identity(r *http.Request) (string, error) {
	raw := r.Header.Get("Cf-Access-Jwt-Assertion")
	if raw == "" {
		return "", errNoToken
	}
	token, err := g.verifier.Verify(r.Context(), raw)
	if err != nil {
		return "", err
	}
	var claims struct {
		Email string `json:"email"`
	}
	if err := token.Claims(&claims); err != nil {
		return "", err
	}
	email := strings.ToLower(strings.TrimSpace(claims.Email))
	if email == "" {
		return "", errNoEmail
	}
	if !admits(email, g.allow) {
		return email, errNotListed
	}
	return email, nil
}

// admits says whether an email gets in: with no list, anyone Access let
// through; otherwise the email itself or its whole domain ("@acme.com").
func admits(email string, allow []string) bool {
	if len(allow) == 0 {
		return true
	}
	at := strings.LastIndex(email, "@")
	return slices.ContainsFunc(allow, func(entry string) bool {
		entry = strings.ToLower(strings.TrimSpace(entry))
		if strings.HasPrefix(entry, "@") {
			return at >= 0 && email[at:] == entry
		}
		return entry == email
	})
}

// handler: door's routes (the web app, agentd's API, the viewer), behind the
// Access token; agentd is told the email in X-Agent-User.
func handler(g gate, novnc *url.URL, agentdSocket, webDir string) http.Handler {
	return door.Handler(door.Config{
		Identify: func(r *http.Request) (door.Caller, error) {
			email, err := g.identity(r)
			return door.Caller{Name: email, Headers: map[string]string{"X-Agent-User": email}}, err
		},
		Agentd:  agentdSocket,
		NoVNC:   novnc,
		Web:     webDir,
		SignOut: "/cdn-cgi/access/logout",
	})
}

// listen opens "unix:/path" or "host:port" (localhost only).
func listen(address string) (net.Listener, error) {
	if path, ok := strings.CutPrefix(address, "unix:"); ok {
		_ = os.Remove(path)
		ln, err := net.Listen("unix", path)
		if err != nil {
			return nil, err
		}
		// cloudflared is in the socket's group.
		return ln, os.Chmod(path, 0o660)
	}
	host, _, err := net.SplitHostPort(address)
	if err != nil {
		return nil, err
	}
	if ip := net.ParseIP(host); host != "localhost" && (ip == nil || !ip.IsLoopback()) {
		return nil, fmt.Errorf("%s: only localhost or a Unix socket", address)
	}
	return net.Listen("tcp", address)
}

func main() {
	configPath := flag.String("config", "/run/front-door/access.json", "Access settings (team_domain, aud, allow)")
	address := flag.String("listen", "127.0.0.1:8080", "where cloudflared connects: localhost host:port or unix:/path")
	novnc := flag.String("novnc", "http://127.0.0.1:6080", "noVNC/websockify to proxy to")
	agentd := flag.String("agentd", "/run/agentd/agentd.sock", "agentd's Unix socket")
	web := flag.String("web", "", "the web app's directory (index.html, assets/); empty: none")
	flag.Parse()

	cfg, err := loadConfig(*configPath)
	if err != nil {
		log.Fatalf("settings: %v", err)
	}
	novncURL, err := url.Parse(*novnc)
	if err != nil {
		log.Fatalf("bad -novnc: %v", err)
	}
	ln, err := listen(*address)
	if err != nil {
		log.Fatalf("listening: %v", err)
	}
	g := newGate(context.Background(), cfg)
	log.Printf("serving %s for Access team %s (allow list: %d entries)", *address, issuer(cfg.TeamDomain), len(cfg.Allow))
	srv := &http.Server{
		Handler:           handler(g, novncURL, *agentd, *web),
		ReadHeaderTimeout: 10 * time.Second,
	}
	log.Fatal(srv.Serve(ln))
}
