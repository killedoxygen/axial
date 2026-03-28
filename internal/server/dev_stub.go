//go:build !dev

package server

const devMode = false

func registerDevRoutes(_ *Server) {}
