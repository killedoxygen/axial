package modloaders

import (
	"sync"
	"time"
)

type cacheEntry struct {
	data      any
	fetchedAt time.Time
}

// MetaCache is a simple in-memory TTL cache for loader metadata.
// It returns stale data when a network error occurs, so the UI
// degrades gracefully on connectivity loss.
type MetaCache struct {
	mu      sync.RWMutex
	entries map[string]cacheEntry
	ttl     time.Duration
}

// NewMetaCache creates a cache with the given time-to-live.
func NewMetaCache(ttl time.Duration) *MetaCache {
	return &MetaCache{
		entries: make(map[string]cacheEntry),
		ttl:     ttl,
	}
}

// Get returns a cached value and whether it's still fresh.
// If the entry exists but is stale, it is still returned (ok=true, fresh=false)
// so callers can use it as a fallback on network errors.
func (c *MetaCache) Get(key string) (data any, ok bool, fresh bool) {
	c.mu.RLock()
	defer c.mu.RUnlock()
	e, exists := c.entries[key]
	if !exists {
		return nil, false, false
	}
	return e.data, true, time.Since(e.fetchedAt) < c.ttl
}

// Set stores a value in the cache.
func (c *MetaCache) Set(key string, data any) {
	c.mu.Lock()
	c.entries[key] = cacheEntry{data: data, fetchedAt: time.Now()}
	c.mu.Unlock()
}

// Delete removes a cached entry.
func (c *MetaCache) Delete(key string) {
	c.mu.Lock()
	delete(c.entries, key)
	c.mu.Unlock()
}

func cacheGetAs[T any](cache *MetaCache, key string) (value T, ok bool, fresh bool) {
	data, ok, fresh := cache.Get(key)
	if !ok {
		return value, false, false
	}
	typed, typeOK := data.(T)
	if !typeOK {
		cache.Delete(key)
		return value, false, false
	}
	return typed, true, fresh
}
