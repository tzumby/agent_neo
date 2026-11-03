// devedge stands in for Cloudflare's edge when trying the web app on a Mac
// (apps/web/dev/run.sh): an Access "team" with its own signing key, and a
// proxy that signs each browser request in as -email and passes it to
// front-door with Cf-Access-Jwt-Assertion, as Access does. For local
// development only; it's never installed on a desktop.
package main

import (
	"crypto/rand"
	"crypto/rsa"
	"encoding/json"
	"flag"
	"log"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"time"

	"github.com/go-jose/go-jose/v4"
)

func main() {
	listen := flag.String("listen", "127.0.0.1:8088", "where the browser connects")
	team := flag.String("team", "127.0.0.1:8089", "the fake Access team (its signing keys)")
	upstream := flag.String("upstream", "http://127.0.0.1:8090", "front-door")
	email := flag.String("email", "you@example.com", "who the browser is signed in as")
	config := flag.String("write-config", "", "write front-door's -config for this team here")
	flag.Parse()

	key, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		log.Fatal(err)
	}
	issuer := "http://" + *team
	if *config != "" {
		settings, _ := json.Marshal(map[string]any{"team_domain": issuer, "aud": "dev", "allow": []string{}})
		if err := os.WriteFile(*config, settings, 0o600); err != nil {
			log.Fatal(err)
		}
	}
	go func() {
		keys := jose.JSONWebKeySet{Keys: []jose.JSONWebKey{{Key: &key.PublicKey, KeyID: "dev", Algorithm: "RS256", Use: "sig"}}}
		mux := http.NewServeMux()
		mux.HandleFunc("/cdn-cgi/access/certs", func(w http.ResponseWriter, _ *http.Request) {
			_ = json.NewEncoder(w).Encode(keys)
		})
		log.Fatal(http.ListenAndServe(*team, mux))
	}()

	signer, err := jose.NewSigner(jose.SigningKey{Algorithm: jose.RS256, Key: jose.JSONWebKey{Key: key, KeyID: "dev"}},
		(&jose.SignerOptions{}).WithType("JWT"))
	if err != nil {
		log.Fatal(err)
	}
	token := func() string {
		now := time.Now()
		claims, _ := json.Marshal(map[string]any{
			"iss": issuer, "aud": []string{"dev"}, "email": *email, "sub": "dev",
			"iat": now.Unix(), "exp": now.Add(time.Hour).Unix(),
		})
		signed, err := signer.Sign(claims)
		if err != nil {
			log.Fatal(err)
		}
		compact, _ := signed.CompactSerialize()
		return compact
	}

	target, err := url.Parse(*upstream)
	if err != nil {
		log.Fatal(err)
	}
	proxy := &httputil.ReverseProxy{
		Rewrite: func(pr *httputil.ProxyRequest) {
			pr.SetURL(target)
			pr.Out.Host = pr.In.Host // as cloudflared does: the public hostname
			pr.Out.Header.Set("Cf-Access-Jwt-Assertion", token())
		},
		FlushInterval: -1,
	}
	log.Printf("the web app as %s: http://%s/", *email, *listen)
	log.Fatal(http.ListenAndServe(*listen, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/cdn-cgi/access/logout" {
			_, _ = w.Write([]byte("Signed out (devedge: sign-out is Cloudflare's)."))
			return
		}
		proxy.ServeHTTP(w, r)
	})))
}
