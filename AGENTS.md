# Repository Instructions

I'm on Nix on Darwin or NixOS on Linux, so use devenv. If `DEVENV_ROOT` is
already set, run commands directly without entering another devenv shell.

`cargo test` accepts at most one positional test filter before `--`; run separate
commands for multiple filters or use a broader single substring.
