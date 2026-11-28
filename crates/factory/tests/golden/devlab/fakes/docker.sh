# The world's docker: a container runs while its name is a line of world/containers.
w='@WORLD@'
if [ "$1" = inspect ] && [ "$2" = -f ] && [ "$3" = '{{.State.Running}}' ]; then
  if grep -qxF "$4" "$w/containers"; then echo true; exit 0; fi
  echo
  echo "Error: No such object: $4" >&2
  exit 1
fi
# ps -aq --filter label=com.supabase.cli.project=<id>: the stack's containers, by name.
if [ "$1" = ps ] && [ "$2" = -aq ] && [ "$3" = --filter ]; then
  id=${4#label=com.supabase.cli.project=}
  grep -E "^supabase_[a-z_]+_${id}\$" "$w/containers"
  exit 0
fi
# update --restart=no <containers>: they no longer come back after a reboot.
if [ "$1" = update ] && [ "$2" = --restart=no ]; then
  exit 0
fi
echo "fake docker: no script for: $*" >&2
exit 127
