package modloaders

import "sync"

var (
	mu       sync.RWMutex
	registry = map[LoaderType]Loader{}
)

// Register adds a loader to the global registry.
func Register(l Loader) {
	mu.Lock()
	registry[l.Type()] = l
	mu.Unlock()
}

// Get returns a registered loader by type.
func Get(t LoaderType) (Loader, bool) {
	mu.RLock()
	defer mu.RUnlock()
	l, ok := registry[t]
	return l, ok
}

// All returns every registered loader.
func All() []Loader {
	mu.RLock()
	defer mu.RUnlock()
	out := make([]Loader, 0, len(registry))
	for _, l := range registry {
		out = append(out, l)
	}
	return out
}

// AllInfo returns display metadata for every registered loader.
func AllInfo() []LoaderInfo {
	mu.RLock()
	defer mu.RUnlock()
	out := make([]LoaderInfo, 0, len(registry))
	for _, l := range registry {
		out = append(out, l.Info())
	}
	return out
}
