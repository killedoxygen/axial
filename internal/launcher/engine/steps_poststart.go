package engine

import "log"

type startProfilerStep struct{}

func (s *startProfilerStep) Name() string { return "start profiler" }

func (s *startProfilerStep) Execute(ctx *LaunchContext) error {
	profile := NewBootProfile(
		ctx.SessionID, ctx.Opts.VersionID, ctx.Process.PID(),
		ctx.EffectivePreset, ctx.EffectiveMaxMemoryMB,
		bootCPUCap(), len(ctx.CDSArgs) > 0,
	)
	profile.Start()
	ctx.Process.Profile = profile
	return nil
}

type scheduleCDSStep struct{}

func (s *scheduleCDSStep) Name() string { return "schedule CDS" }

func (s *scheduleCDSStep) Execute(ctx *LaunchContext) error {
	if !ctx.IsModded && ctx.EffectiveJavaMajor >= 11 && len(ctx.CDSArgs) == 0 {
		javaPath := ctx.JavaRuntime.EffectivePath
		classpath := ctx.Classpath
		configDir := ctx.ConfigDir
		versionID := ctx.Opts.VersionID
		go func() {
			archivePath := CDSArchivePath(configDir, versionID)
			if err := GenerateCDSArchive(javaPath, classpath, archivePath); err != nil {
				log.Printf("CDS archive generation failed for %s: %v", versionID, err)
			}
		}()
	}

	if len(ctx.CDSArgs) > 0 {
		configDir := ctx.ConfigDir
		versionID := ctx.Opts.VersionID
		gp := ctx.Process
		go func() {
			<-gp.Done()
			if gp.CDSFailed {
				log.Printf("CDS archive unusable for %s, invalidating for next launch", versionID)
				InvalidateCDSArchive(configDir, versionID)
			}
		}()
	}

	return nil
}
