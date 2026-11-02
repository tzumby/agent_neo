// tailnet-proxy is the desktop's only presence on your tailnet.
//
// It embeds Tailscale (tsnet) instead of running tailscaled, so only this
// process joins the tailnet: the rest of the VM, including commands run by the
// agent, can't reach other tailnet devices or change Tailscale settings.
//
// On https://<hostname>.<tailnet>.ts.net (Tailscale certificate) it serves:
//   - /api/*  the agent's API (agentd, on a Unix socket), with the caller's
//     Tailscale identity in X-Tailscale-User / X-Tailscale-Node
//   - anything else: the noVNC viewer (browser fallback)
//
// On port 5900 it forwards raw VNC to x11vnc, for the apps' native viewer.
//
// Every connection is checked against the tailnet identity (WhoIs) and the
// -allow list of logins or the -allow-tags list of device tags (a tester's
// devices join the desktops tailnet with a key, tagged, without a login). The
// first run needs an auth key in TS_AUTHKEY; the node's identity is then kept
// in -state-dir.
package main

import (
	"context"
	"errors"
	"flag"
	"io"
	"log"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"slices"
	"strings"
	"time"

	"tailscale.com/client/local"
	"tailscale.com/tsnet"
)

type gate struct {
	who         *local.Client
	allowed     []string
	allowedTags []string
}

// admits says whether a caller gets in: with neither list set, whoever the
// tailnet policy lets through; otherwise a login on the list, or a device
// with one of the tags.
func admits(login string, tags, allowed, allowedTags []string) bool {
	if len(allowed) == 0 && len(allowedTags) == 0 {
		return true
	}
	if slices.Contains(allowed, login) {
		return true
	}
	return slices.ContainsFunc(tags, func(tag string) bool { return slices.Contains(allowedTags, tag) })
}

// identity returns the caller's Tailscale login (for a tagged device, its
// tags) and device, or an error if they aren't on the tailnet or not allowed.
func (g gate) identity(ctx context.Context, remoteAddr string) (login, node string, err error) {
	peer, err := g.who.WhoIs(ctx, remoteAddr)
	if err != nil {
		return "", "", err
	}
	login, node = peer.UserProfile.LoginName, peer.Node.ComputedName
	tags := peer.Node.Tags
	if !admits(login, tags, g.allowed, g.allowedTags) {
		return login, node, errNotAllowed
	}
	if len(tags) > 0 {
		login = strings.Join(tags, ",") // "tagged-devices" says nothing
	}
	return login, node, nil
}

var errNotAllowed = errors.New("not on the allowlist")

func main() {
	hostname := flag.String("hostname", "cloud-desktop", "device name on the tailnet")
	stateDir := flag.String("state-dir", "/var/lib/tailnet-proxy", "where the node keeps its identity")
	novnc := flag.String("novnc", "http://127.0.0.1:6080", "noVNC/websockify to proxy to")
	agentd := flag.String("agentd", "/run/agentd/agentd.sock", "agentd's Unix socket")
	vnc := flag.String("vnc", "127.0.0.1:5900", "x11vnc to forward port 5900 to")
	allow := flag.String("allow", "", "comma-separated Tailscale login names allowed in (default: whoever the tailnet policy lets through)")
	allowTags := flag.String("allow-tags", "", "comma-separated device tags allowed in, e.g. tag:user-alex (as well as -allow)")
	flag.Parse()

	novncURL, err := url.Parse(*novnc)
	if err != nil {
		log.Fatalf("bad -novnc: %v", err)
	}
	allowed, allowedTags := list(*allow), list(*allowTags)

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
	who, err := srv.LocalClient()
	if err != nil {
		log.Fatalf("local client: %v", err)
	}
	g := gate{who: who, allowed: allowed, allowedTags: allowedTags}

	vncLn, err := srv.Listen("tcp", ":5900")
	if err != nil {
		log.Fatalf("listening on 5900: %v", err)
	}
	go serveVNC(vncLn, g, *vnc)

	webLn, err := srv.ListenTLS("tcp", ":443")
	if err != nil {
		log.Fatalf("listening on 443: %v", err)
	}
	log.Printf("serving https://%s (api, noVNC) and port 5900 (VNC)", strings.Join(srv.CertDomains(), ", https://"))
	log.Fatal(http.Serve(webLn, webHandler(g, novncURL, *agentd)))
}

func webHandler(g gate, novnc *url.URL, agentdSocket string) http.Handler {
	viewer := httputil.NewSingleHostReverseProxy(novnc)
	api := &httputil.ReverseProxy{
		Rewrite: func(pr *httputil.ProxyRequest) {
			pr.SetURL(&url.URL{Scheme: "http", Host: "agentd"})
			pr.Out.Host = "agentd"
		},
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				return (&net.Dialer{}).DialContext(ctx, "unix", agentdSocket)
			},
		},
		FlushInterval: -1, // stream Server-Sent Events as they come
	}
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		login, node, err := g.identity(r.Context(), r.RemoteAddr)
		if err != nil {
			log.Printf("denied %s %s to %q on %q: %v", r.Method, r.URL.Path, login, node, err)
			http.Error(w, "not allowed", http.StatusForbidden)
			return
		}
		if strings.HasPrefix(r.URL.Path, "/api/") {
			// Only we set these; never trust a client's copy.
			for name := range r.Header {
				if strings.HasPrefix(strings.ToLower(name), "x-tailscale-") {
					r.Header.Del(name)
				}
			}
			r.Header.Set("X-Tailscale-User", login)
			r.Header.Set("X-Tailscale-Node", node)
			api.ServeHTTP(w, r)
			return
		}
		if r.URL.Path == "/websockify" {
			log.Printf("viewer session opened by %s on %s", login, node)
		}
		viewer.ServeHTTP(w, r)
	})
}

func serveVNC(ln net.Listener, g gate, target string) {
	for {
		conn, err := ln.Accept()
		if err != nil {
			log.Printf("vnc accept: %v", err)
			return
		}
		go func() {
			defer conn.Close()
			ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
			login, node, err := g.identity(ctx, conn.RemoteAddr().String())
			cancel()
			if err != nil {
				log.Printf("denied VNC to %q on %q: %v", login, node, err)
				return
			}
			upstream, err := net.DialTimeout("tcp", target, 5*time.Second)
			if err != nil {
				log.Printf("vnc: %v", err)
				return
			}
			defer upstream.Close()
			log.Printf("VNC session opened by %s on %s", login, node)
			done := make(chan struct{}, 2)
			go func() { io.Copy(upstream, conn); done <- struct{}{} }()
			go func() { io.Copy(conn, upstream); done <- struct{}{} }()
			<-done
			log.Printf("VNC session closed for %s on %s", login, node)
		}()
	}
}

// list splits a comma-separated flag, dropping blanks.
func list(flag string) []string {
	var items []string
	for _, item := range strings.Split(flag, ",") {
		if item = strings.TrimSpace(item); item != "" {
			items = append(items, item)
		}
	}
	return items
}
