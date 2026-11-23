# The world's docker: a container runs while its name is a line of world/containers.
w='@WORLD@'
if [ "$1" = inspect ] && [ "$2" = -f ] && [ "$3" = '{{.State.Running}}' ]; then
  if grep -qxF "$4" "$w/containers"; then echo true; exit 0; fi
  echo
  echo "Error: No such object: $4" >&2
  exit 1
fi
echo "fake docker: no script for: $*" >&2
exit 127
