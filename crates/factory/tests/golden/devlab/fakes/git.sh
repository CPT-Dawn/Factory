# No worktree is a git repository, so devlab takes its working directory as the worktree.
echo "fatal: not a git repository (or any of the parent directories): .git" >&2
exit 128
