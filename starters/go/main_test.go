package main

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"
)

type failingSource struct{ err error }

func (f failingSource) Search(context.Context, Query) ([]BookMetadata, error) { return nil, f.err }
func (f failingSource) Lookup(context.Context, string) (*BookMetadata, error) { return nil, f.err }

func TestSearchAnswersWithAMatchesList(t *testing.T) {
	rec := httptest.NewRecorder()
	handleSearch(rec, httptest.NewRequest(http.MethodGet, "/search?query=earthsea", nil), newExampleSource())

	var body struct{ Matches []BookMetadata }
	if err := json.NewDecoder(rec.Body).Decode(&body); err != nil || rec.Code != http.StatusOK {
		t.Fatalf("status %d, err %v", rec.Code, err)
	}
	if len(body.Matches) != 1 || body.Matches[0].ProviderID != "ex-1" {
		t.Fatalf("matches = %+v", body.Matches)
	}
}

func TestSearchWithNothingFoundIsAnEmptyListNotNull(t *testing.T) {
	rec := httptest.NewRecorder()
	handleSearch(rec, httptest.NewRequest(http.MethodGet, "/search?query=nothing-like-this", nil), newExampleSource())
	if got := rec.Body.String(); got != "{\"matches\":[]}\n" {
		t.Fatalf("body = %q", got)
	}
}

func TestLookupAnswers404ForAnUnknownID(t *testing.T) {
	mux := http.NewServeMux()
	source := newExampleSource()
	mux.HandleFunc("GET /book/{id}", func(w http.ResponseWriter, r *http.Request) { handleLookup(w, r, source) })
	rec := httptest.NewRecorder()
	mux.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/book/missing", nil))
	if rec.Code != http.StatusNotFound {
		t.Fatalf("status = %d", rec.Code)
	}
}

func TestSourceErrorsMapToTheStatusCodesTheContractDefines(t *testing.T) {
	cases := map[error]int{
		&RateLimitedError{RetryAfterSeconds: 30}: http.StatusTooManyRequests,
		context.DeadlineExceeded:                 http.StatusBadGateway,
	}
	for err, want := range cases {
		rec := httptest.NewRecorder()
		handleSearch(rec, httptest.NewRequest(http.MethodGet, "/search?query=x", nil), failingSource{err})
		if rec.Code != want {
			t.Fatalf("%v: status = %d, want %d", err, rec.Code, want)
		}
	}
}
