package store

import "fmt"

const Version = "1"

var Default = New()

type Store struct {
	items map[string]string
}

type Getter interface {
	Get(key string) string
}

type ID string

func New() *Store {
	return &Store{items: map[string]string{}}
}

func (s *Store) Get(key string) string {
	return s.items[key]
}

func (s Store) Close() {
	fmt.Println("closed")
}
