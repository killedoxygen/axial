package modrinth

import (
	"context"
	"sync"
	"time"
)

// Limiter is a simple token bucket rate limiter.
type Limiter struct {
	tokens     float64
	maxTokens  float64
	refillRate float64
	mu         sync.Mutex
	last       time.Time
}

// NewLimiter creates a limiter allowing rps requests per second.
func NewLimiter(rps float64) *Limiter {
	if rps <= 0 {
		rps = 1
	}
	now := time.Now()
	return &Limiter{
		tokens:     rps,
		maxTokens:  rps,
		refillRate: rps / float64(time.Second),
		last:       now,
	}
}

// Wait blocks until a token is available.
func (l *Limiter) Wait(ctx context.Context) error {
	for {
		l.mu.Lock()
		now := time.Now()
		elapsed := now.Sub(l.last)
		l.last = now
		l.tokens += float64(elapsed) * l.refillRate
		if l.tokens > l.maxTokens {
			l.tokens = l.maxTokens
		}
		if l.tokens >= 1 {
			l.tokens--
			l.mu.Unlock()
			return nil
		}

		needed := 1 - l.tokens
		wait := time.Duration(needed / l.refillRate)
		if wait < time.Millisecond {
			wait = time.Millisecond
		}
		l.mu.Unlock()

		timer := time.NewTimer(wait)
		select {
		case <-ctx.Done():
			timer.Stop()
			return ctx.Err()
		case <-timer.C:
		}
	}
}
