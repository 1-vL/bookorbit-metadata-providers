package main

import (
	"context"
	"fmt"
	"strings"
)

// BookMetadata is one record in the spec's shape. Fields after the blank line are optional
// extensions BookOrbit reads; other apps ignore them.
type BookMetadata struct {
	ProviderID    string   `json:"providerId"`
	MediaType     string   `json:"mediaType,omitempty"`
	Title         string   `json:"title"`
	Subtitle      string   `json:"subtitle,omitempty"`
	Authors       []string `json:"authors,omitempty"`
	Narrators     []string `json:"narrators,omitempty"`
	Publisher     string   `json:"publisher,omitempty"`
	PublishedYear string   `json:"publishedYear,omitempty"`
	Description   string   `json:"description,omitempty"`
	Cover         string   `json:"cover,omitempty"`
	ISBN13        string   `json:"isbn13,omitempty"`
	ISBN10        string   `json:"isbn10,omitempty"`
	Genres        []string `json:"genres,omitempty"`
	Series        []Series `json:"series,omitempty"`
	LanguageCode  string   `json:"languageCode,omitempty"`
	Pages         int      `json:"pages,omitempty"`

	Rating      *float64 `json:"rating,omitempty"`
	RatingCount *int     `json:"ratingCount,omitempty"`
	URL         string   `json:"url,omitempty"`
}

type Series struct {
	Series         string   `json:"series"`
	SequenceNumber *float64 `json:"sequenceNumber,omitempty"`
}

// Query is what a client asked for, already trimmed.
type Query struct {
	Title, Author, ISBN, MediaType string
	Limit                          int
}

// RateLimitedError tells the client to back off instead of counting the call as a failure.
type RateLimitedError struct{ RetryAfterSeconds int }

func (e *RateLimitedError) Error() string {
	return fmt.Sprintf("rate limited for %ds", e.RetryAfterSeconds)
}

// Source is the only thing you write. Search returns matches best first; Lookup returns nil for an
// id that does not exist. Return an error for anything else that goes wrong, never an empty list:
// an empty list tells the client the book is not there.
type Source interface {
	Search(ctx context.Context, q Query) ([]BookMetadata, error)
	Lookup(ctx context.Context, id string) (*BookMetadata, error)
}

// exampleSource answers from a list in memory. Replace it with calls to your upstream.
type exampleSource struct{ books []BookMetadata }

func newExampleSource() *exampleSource {
	rating, count := 4.2, 1200
	one := 1.0
	return &exampleSource{books: []BookMetadata{
		{
			ProviderID:    "ex-1",
			MediaType:     "book",
			Title:         "A Wizard of Earthsea",
			Authors:       []string{"Ursula K. Le Guin"},
			PublishedYear: "1968",
			Series:        []Series{{Series: "Earthsea Cycle", SequenceNumber: &one}},
			Rating:        &rating,
			RatingCount:   &count,
			URL:           "https://example.org/books/ex-1",
		},
	}}
}

func (s *exampleSource) Search(_ context.Context, q Query) ([]BookMetadata, error) {
	var out []BookMetadata
	for _, book := range s.books {
		if q.Title != "" && !strings.Contains(strings.ToLower(book.Title), strings.ToLower(q.Title)) {
			continue
		}
		out = append(out, book)
		if len(out) == q.Limit {
			break
		}
	}
	return out, nil
}

func (s *exampleSource) Lookup(_ context.Context, id string) (*BookMetadata, error) {
	for _, book := range s.books {
		if book.ProviderID == id {
			return &book, nil
		}
	}
	return nil, nil
}
