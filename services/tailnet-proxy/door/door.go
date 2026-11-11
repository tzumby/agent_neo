// Package door is what the desktop's way in serves once it knows who's
// calling: tailnet-proxy (Tailscale) on the desktop, and devdoor for trying
// the web app on a Mac.
//
//   - /api/*           agentd's API (its Unix socket), told who's calling
//   - /session         who's signed in, for the web app
//   - /viewer/*        the noVNC viewer (take over), and /websockify, its stream
//   - / and /assets/*  the web app, under a strict Content-Security-Policy
//
// Requests that change something must come from the web app's own pages
// (Sec-Fetch-Site, or Origin against Host), and so must the viewer's
// WebSocket: another site can't borrow the person's session to act on the
// desktop. Clients that aren't browsers (curl, scripts) send neither header
// and pass, but still had to get through Identify.
package door

import (
	"context"
	"encoding/json"
	"log"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"strings"
)

// Caller is who a request comes from, as the way in established it.
type Caller struct {
	// Who, for the web app and the log: a Tailscale login.
	Name string
	// What agentd is told: X-Tailscale-User and -Node.
	Headers map[string]string
}

// Identify says who a request is from, or why it isn't let in (the Caller
// may still name who was refused, for the log).
type Identify func(*http.Request) (Caller, error)

type Config struct {
	Identify Identify
	// agentd's Unix socket.
	Agentd string
	// noVNC/websockify.
	NoVNC *url.URL
	// The web app's directory (index.html, assets/). Empty: none.
	Web string
}

// WebPolicy: the web app's pages may load only their own files and talk only
// to this host; nothing may frame them.
const WebPolicy = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; " +
	"font-src 'self'; connect-src 'self'; frame-src 'self'; frame-ancestors 'none'; base-uri 'none'; " +
	"form-action 'self'; object-src 'none'"

// webApp serves the web app: / is its page, /assets/* its files. No
// directory listings, nothing else.
func webApp(dir string) http.Handler {
	files := http.FileServer(http.Dir(dir))
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/" && (!strings.HasPrefix(r.URL.Path, "/assets/") || strings.HasSuffix(r.URL.Path, "/")) {
			http.NotFound(w, r)
			return
		}
		h := w.Header()
		h.Set("Content-Security-Policy", WebPolicy)
		h.Set("X-Content-Type-Options", "nosniff")
		h.Set("Referrer-Policy", "no-referrer")
		h.Set("Cache-Control", "no-cache") // small files; revalidated, so a deploy shows at once
		files.ServeHTTP(w, r)
	})
}

// sameSiteSocket says whether a WebSocket handshake comes from this host's
// own pages. Browsers always send Origin on one; another site's page opening
// the viewer's stream with the person's session is refused.
func sameSiteSocket(r *http.Request) bool {
	if !strings.EqualFold(r.Header.Get("Upgrade"), "websocket") {
		return true
	}
	if site := r.Header.Get("Sec-Fetch-Site"); site != "" && site != "same-origin" {
		return false
	}
	origin := r.Header.Get("Origin")
	if origin == "" {
		return true // not a browser, so no borrowed session; it still had to be identified
	}
	u, err := url.Parse(origin)
	return err == nil && u.Host == r.Host
}

// Handler checks every request, then sends it to agentd, the viewer or the
// web app.
func Handler(cfg Config) http.Handler {
	viewer := httputil.NewSingleHostReverseProxy(cfg.NoVNC)
	api := &httputil.ReverseProxy{
		Rewrite: func(pr *httputil.ProxyRequest) {
			pr.SetURL(&url.URL{Scheme: "http", Host: "agentd"})
			pr.Out.Host = "agentd"
		},
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				return (&net.Dialer{}).DialContext(ctx, "unix", cfg.Agentd)
			},
		},
		FlushInterval: -1, // stream Server-Sent Events as they come
	}
	var web http.Handler = http.NotFoundHandler()
	if cfg.Web != "" {
		web = webApp(cfg.Web)
	}
	routes := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		who, err := cfg.Identify(r)
		if err != nil {
			log.Printf("denied %s %s to %q: %v", r.Method, r.URL.Path, who.Name, err)
			http.Error(w, "not allowed", http.StatusForbidden)
			return
		}
		// Only we say who's calling; and nothing behind us needs the
		// browser's cookies.
		for name := range r.Header {
			if strings.HasPrefix(strings.ToLower(name), "x-tailscale-") {
				r.Header.Del(name)
			}
		}
		r.Header.Del("Cookie")
		if !sameSiteSocket(r) {
			log.Printf("denied a WebSocket to %s from %q", who.Name, r.Header.Get("Origin"))
			http.Error(w, "not allowed", http.StatusForbidden)
			return
		}
		switch path := r.URL.Path; {
		case strings.HasPrefix(path, "/api/"):
			for name, value := range who.Headers {
				r.Header.Set(name, value)
			}
			api.ServeHTTP(w, r)
		case path == "/session":
			w.Header().Set("Content-Type", "application/json")
			w.Header().Set("Cache-Control", "no-store")
			_ = json.NewEncoder(w).Encode(map[string]string{"email": who.Name})
		case path == "/websockify":
			log.Printf("viewer session opened by %s", who.Name)
			viewer.ServeHTTP(w, r)
		case path == "/viewer":
			http.Redirect(w, r, "/viewer/", http.StatusFound)
		case strings.HasPrefix(path, "/viewer/"):
			// Framed by the web app only.
			w.Header().Set("X-Frame-Options", "SAMEORIGIN")
			w.Header().Set("Content-Security-Policy", "frame-ancestors 'self'")
			http.StripPrefix("/viewer", viewer).ServeHTTP(w, r)
		default:
			web.ServeHTTP(w, r)
		}
	})
	// POST, PUT, DELETE… only from this host's own pages.
	return http.NewCrossOriginProtection().Handler(routes)
}
