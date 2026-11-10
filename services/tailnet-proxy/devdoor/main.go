// devdoor serves what tailnet-proxy does (package door: the web app, agentd's
// API, the viewer) on localhost, with every request signed in as -login, for
// trying the web app on a Mac (apps/web/dev/run.sh). For local development
// only: it trusts whoever can reach its port, so it's never installed on a
// desktop.
package main

import (
	"flag"
	"log"
	"net/http"
	"net/url"

	"cloud-desktops/tailnet-proxy/door"
)

func main() {
	listen := flag.String("listen", "127.0.0.1:8088", "where the browser connects (keep it on localhost)")
	login := flag.String("login", "you@example.com", "who every request is signed in as")
	agentd := flag.String("agentd", "", "agentd's Unix socket")
	novnc := flag.String("novnc", "http://127.0.0.1:6080", "noVNC/websockify")
	web := flag.String("web", "apps/web/public", "the web app's directory")
	flag.Parse()

	novncURL, err := url.Parse(*novnc)
	if err != nil {
		log.Fatalf("bad -novnc: %v", err)
	}
	log.Printf("the web app as %s: http://%s/", *login, *listen)
	log.Fatal(http.ListenAndServe(*listen, door.Handler(door.Config{
		Identify: func(*http.Request) (door.Caller, error) {
			return door.Caller{Name: *login, Headers: map[string]string{"X-Tailscale-User": *login, "X-Tailscale-Node": "devdoor"}}, nil
		},
		Agentd: *agentd,
		NoVNC:  novncURL,
		Web:    *web,
	})))
}
