package store

type Store struct{}

func (s *Store) Get(key string) string {
	return key
}

func (s *Store) Put(key string) {
}
