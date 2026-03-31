package modloaders

import "sync"

var installLocks sync.Map

func withInstallLock[T any](key string, fn func() (T, error)) (T, error) {
	lockAny, _ := installLocks.LoadOrStore(key, &sync.Mutex{})
	lock := lockAny.(*sync.Mutex)
	lock.Lock()
	defer lock.Unlock()
	return fn()
}
