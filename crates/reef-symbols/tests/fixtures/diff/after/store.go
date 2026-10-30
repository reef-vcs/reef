package store

type Store struct{}

func (s *Store) Get(key string) string {
	return key
}
