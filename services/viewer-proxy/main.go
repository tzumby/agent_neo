// viewer-proxy puts the desktop's noVNC viewer on your tailnet, and nowhere else.
//
// It embeds Tailscale (tsnet) instead of running tailscaled, so only this
// process joins the tailnet: the rest of the VM, including commands run by the
// agent, can't reach other tailnet devices or change Tailscale settings.
// It serves https://<hostname>.<tailnet>.ts.net with a Tailscale certificate
// and reverse-proxies to websockify/noVNC on localhost (WebSockets included).
//
// The first run needs an auth key in TS_AUTHKEY; the node's identity is then
// kept in -state-dir.
package main

import (
	"context"
	"flag"
	"log"
	"net/http"
	"net/http/httputil"
	"net/url"
	"slices"
	"strings"
	"time"

	"tailscale.com/tsnet"
)

func main() {
	hostname := flag.String("hostname", "cloud-desktop", "device name on the tailnet")
	stateDir := flag.String("state-dir", "/var/lib/viewer-proxy", "where the node keeps its identity")
	target := flag.String("target", "http://127.0.0.1:6080", "noVNC/websockify to proxy to")
	allow := flag.String("allow", "", "comma-separated Tailscale login names allowed in (default: whoever the tailnet policy lets through)")
	flag.Parse()

	upstream, err := url.Parse(*target)
	if err != nil {
		log.Fatalf("bad -target: %v", err)
	}
	var allowed []string
	for _, name := range strings.Split(*allow, ",") {
		if name = strings.TrimSpace(name); name != "" {
			allowed = append(allowed, name)
		}
	}

	srv := &tsnet.Server{
		Hostname: *hostname,
		Dir:      *stateDir,
		UserLogf: log.Printf,
		Logf:     func(string, ...any) {}, // tsnet's backend logs are very chatty
	}
	defer srv.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
	if _, err := srv.Up(ctx); err != nil {
		log.Fatalf("joining the tailnet: %v", err)
	}
	cancel()
	ln, err := srv.ListenTLS("tcp", ":443")
	if err != nil {
		log.Fatalf("listening: %v", err)
	}
	who, err := srv.LocalClient()
	if err != nil {
		log.Fatalf("local client: %v", err)
	}

	proxy := httputil.NewSingleHostReverseProxy(upstream)
	handler := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		peer, err := who.WhoIs(r.Context(), r.RemoteAddr)
		if err != nil {
			http.Error(w, "unknown peer", http.StatusForbidden)
			return
		}
		login := peer.UserProfile.LoginName
		if len(allowed) > 0 && !slices.Contains(allowed, login) {
			log.Printf("denied %s %s to %s on %s", r.Method, r.URL.Path, login, peer.Node.ComputedName)
			http.Error(w, "not allowed", http.StatusForbidden)
			return
		}
		if r.URL.Path == "/websockify" {
			log.Printf("viewer session opened by %s on %s", login, peer.Node.ComputedName)
		}
		proxy.ServeHTTP(w, r)
	})

	log.Printf("serving %s at https://%s/vnc.html", *target, strings.Join(srv.CertDomains(), ", https://"))
	log.Fatal(http.Serve(ln, handler))
}
