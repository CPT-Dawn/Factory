# The project's supabase CLI: start runs the lease's db container, stop removes it.
w='@WORLD@'
case "$1" in
start)
  if grep -qxF 'supabase start' "$w/broken"; then
    echo "failed to start docker container: Bind for 0.0.0.0:$SUPABASE_DB_PORT failed: port is already allocated" >&2
    exit 1
  fi
  echo "supabase_db_$SUPABASE_PROJECT_ID" >> "$w/containers"
  echo "Started supabase local development setup."
  exit 0 ;;
stop)
  if grep -qxF 'supabase stop' "$w/broken"; then
    echo "failed to stop docker container: permission denied" >&2
    exit 1
  fi
  grep -vxF "supabase_db_$3" "$w/containers" > "$w/containers.new"
  mv "$w/containers.new" "$w/containers"
  echo "Stopped supabase local development setup."
  exit 0 ;;
esac
echo "fake supabase: no script for: $*" >&2
exit 127
