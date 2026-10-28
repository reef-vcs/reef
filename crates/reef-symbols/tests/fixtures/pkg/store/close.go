package store

func (s *Store) Flush() {
	s.Close()
}
