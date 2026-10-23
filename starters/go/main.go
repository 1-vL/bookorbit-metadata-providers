// A minimal metadata provider for BookOrbit and other apps that speak the book-metadata-provider
// spec. Replace the source in source.go with your own; this file only serves the contract.
package main

import (
	"context"
	"crypto/subtle"
	"encoding/json"
	"errors"
	"log"
	"net/http"
	"os"
	"strconv"
	"strings"
	"time"
)

const version = "0.1.0"

// Info is GET /provider/info. Only Name is required by the spec; the rest are optional extensions
// BookOrbit reads, and every one has a default if you leave it out.
func info() map[string]any {
	return map[string]any{
		"name":                   "Example Provider",
		"version":                version,
		"authenticationRequired": os.Getenv("AUTH_TOKEN") != "",
		// Permanent: BookOrbit files every id and rating it stores under this key.
		"key":          "example-provider",
		"mediaTypes":   []string{"book"},
		"lookup":       true,
		"searchParams": []string{"isbn", "limit"},
		"rateLimit":    map[string]int{"maxConcurrency": 2, "minIntervalMs": 0},
	}
}

func main() {
	source := newExampleSource()
	mux := http.NewServeMux()
	mux.HandleFunc("GET /provider/info", func(w http.ResponseWriter, r *http.Request) { writeJSON(w, http.StatusOK, info()) })
	mux.HandleFunc("GET /search", requireAuth(func(w http.ResponseWriter, r *http.Request) { handleSearch(w, r, source) }))
	mux.HandleFunc("GET /book/{id}", requireAuth(func(w http.ResponseWriter, r *http.Request) { handleLookup(w, r, source) }))

	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}
	srv := &http.Server{Addr: ":" + port, Handler: mux, ReadHeaderTimeout: 5 * time.Second, WriteTimeout: 30 * time.Second}
	log.Printf("listening on :%s", port)
	log.Fatal(srv.ListenAndServe())
}

func handleSearch(w http.ResponseWriter, r *http.Request, source Source) {
	q := r.URL.Query()
	query := Query{
		Title:     strings.TrimSpace(q.Get("query")),
		Author:    strings.TrimSpace(q.Get("author")),
		ISBN:      strings.TrimSpace(q.Get("isbn")),
		MediaType: q.Get("mediaType"),
		Limit:     clamp(q.Get("limit"), 5, 1, 50),
	}
	if query.Title == "" && query.ISBN == "" {
		writeError(w, http.StatusBadRequest, "query is required")
		return
	}
	ctx, cancel := context.WithTimeout(r.Context(), 20*time.Second)
	defer cancel()
	matches, err := source.Search(ctx, query)
	if err != nil {
		writeSourceError(w, err)
		return
	}
	if matches == nil {
		matches = []BookMetadata{}
	}
	writeJSON(w, http.StatusOK, map[string]any{"matches": matches})
}

func handleLookup(w http.ResponseWriter, r *http.Request, source Source) {
	ctx, cancel := context.WithTimeout(r.Context(), 20*time.Second)
	defer cancel()
	book, err := source.Lookup(ctx, r.PathValue("id"))
	if err != nil {
		writeSourceError(w, err)
		return
	}
	if book == nil {
		writeError(w, http.StatusNotFound, "Not found")
		return
	}
	writeJSON(w, http.StatusOK, book)
}

// Status codes are part of the contract: BookOrbit backs off on 429 and treats 5xx as a failure it
// may answer from its cache, never as "no results".
func writeSourceError(w http.ResponseWriter, err error) {
	var limited *RateLimitedError
	if errors.As(err, &limited) {
		w.Header().Set("Retry-After", strconv.Itoa(limited.RetryAfterSeconds))
		writeError(w, http.StatusTooManyRequests, "rate limited")
		return
	}
	log.Printf("source failed: %v", err)
	writeError(w, http.StatusBadGateway, "source unavailable")
}

func requireAuth(next http.HandlerFunc) http.HandlerFunc {
	token := os.Getenv("AUTH_TOKEN")
	return func(w http.ResponseWriter, r *http.Request) {
		if token != "" && subtle.ConstantTimeCompare([]byte(r.Header.Get("Authorization")), []byte(token)) != 1 {
			writeError(w, http.StatusUnauthorized, "Unauthorized")
			return
		}
		next(w, r)
	}
}

func writeError(w http.ResponseWriter, status int, message string) {
	writeJSON(w, status, map[string]string{"error": message})
}

func writeJSON(w http.ResponseWriter, status int, body any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(body)
}

func clamp(raw string, fallback, min, max int) int {
	n, err := strconv.Atoi(raw)
	if err != nil {
		return fallback
	}
	if n < min {
		return min
	}
	if n > max {
		return max
	}
	return n
}
