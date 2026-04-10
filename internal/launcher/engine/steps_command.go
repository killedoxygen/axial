package engine

import "os/exec"

type prefetchStep struct{}

func (s *prefetchStep) Name() string { return "prefetch" }

func (s *prefetchStep) Execute(ctx *LaunchContext) error {
	prefetchForLaunch(ctx.Libraries, ctx.ClientJarPath, ctx.Opts.MCDir, ctx.Version.AssetIndex.ID)
	return nil
}

type buildCommandStep struct{}

func (s *buildCommandStep) Name() string { return "build command" }

func (s *buildCommandStep) Execute(ctx *LaunchContext) error {
	var cmdArgs []string
	cmdArgs = append(cmdArgs, ctx.CDSArgs...)
	cmdArgs = append(cmdArgs, ctx.BootArgs...)
	cmdArgs = append(cmdArgs, ctx.JVMArgs...)
	cmdArgs = append(cmdArgs, ctx.GCArgs...)
	cmdArgs = append(cmdArgs, ctx.Opts.ExtraJVMArgs...)
	cmdArgs = append(cmdArgs, ctx.MemArgs...)
	cmdArgs = append(cmdArgs, ctx.Version.MainClass)
	cmdArgs = append(cmdArgs, ctx.GameArgs...)
	ctx.CmdArgs = cmdArgs

	cmd := exec.Command(ctx.JavaRuntime.EffectivePath, cmdArgs...)
	cmd.Dir = ctx.GameDir
	setProcAttr(cmd)
	ctx.Cmd = cmd

	return nil
}

type startProcessStep struct{}

func (s *startProcessStep) Name() string { return "start process" }

func (s *startProcessStep) Execute(ctx *LaunchContext) error {
	gp := NewGameProcess(ctx.Cmd, ctx.NativesDir)
	if err := gp.Start(); err != nil {
		CleanupNativesDir(ctx.NativesDir)
		return err
	}
	ctx.Process = gp
	return nil
}
