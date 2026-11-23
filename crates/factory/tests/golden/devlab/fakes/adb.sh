# The world's adb: devices come from world/devices, one "serial state props..." line each.
w='@WORLD@'
row() { awk -v s="$1" '$1 == s' "$w/devices"; }
frame() { awk '{ printf "%s\t%s\n", $1, $2 }' "$w/devices"; }
case "$1" in
devices)
  echo "List of devices attached"
  awk '{ s = $1; $1 = ""; printf "%-22s%s\n", s, $0 }' "$w/devices"
  echo
  exit 0 ;;
start-server)
  exit 0 ;;
track-devices)
  # One frame now and one each time the device list changes: 4 hex digits of length, then the
  # list: "serial<TAB>state" lines, or with -l whole "serial state props..." lines. With -l it
  # also writes the list it streamed last to world/tracked.<pid>, so the harness can wait until
  # a stream has shown a change. It ends with the process that started it.
  long=
  [ "$2" = -l ] && long=1
  [ -n "$long" ] && trap 'rm -f "$w/tracked.$$"' EXIT
  last=x
  while kill -0 "$PPID" 2>/dev/null; do
    if [ -n "$long" ]; then now=$(cat "$w/devices"); else now=$(frame); fi
    if [ "$now" != "$last" ]; then
      body=$(printf '%s' "$now" | awk 'NF { print }')
      [ -n "$body" ] && body="$body
"
      printf '%04x%s' "$(printf '%s' "$body" | wc -c)" "$body"
      [ -n "$long" ] && printf '%s' "$now" > "$w/tracked.$$"
      last=$now
    fi
    sleep 0.05
  done
  exit 0 ;;
-s)
  serial=$2
  shift 2
  state=$(row "$serial" | awk '{ print $2 }')
  if [ -z "$state" ]; then echo "adb: device '$serial' not found" >&2; exit 1; fi
  if [ "$state" != device ]; then echo "adb: device $state" >&2; exit 1; fi
  case "$1" in
  shell)
    # adb runs its arguments as one command line, whether they come as one or several.
    shift
    case "$*" in
    *ro.product.cpu.abi*) case "$serial" in emulator-*) echo x86_64 ;; *) echo arm64-v8a ;; esac ;;
    *sys.boot_completed*) grep -qxF "$serial" "$w/stalled" || echo 1 ;;
    *dumpsys\ window*) grep -qxF "$serial" "$w/locked" && echo "  mDreamingLockscreen=true" ;;
    esac
    exit 0 ;;
  emu)
    # emu kill: the emulator saves its snapshot and exits, and its unit with it.
    if [ "$2" = kill ]; then
      unit=$(awk -v p="${serial#emulator-}" '$2 == p { print $1 }' "$w/unit-ports" | tail -1)
      if [ -n "$unit" ]; then
        grep -vxF "$unit" "$w/units" > "$w/units.new"
        mv "$w/units.new" "$w/units"
      fi
      awk -v s="$serial" '$1 != s' "$w/devices" > "$w/devices.new"
      mv "$w/devices.new" "$w/devices"
    fi
    exit 0 ;;
  reverse)
    exit 0 ;;
  esac ;;
esac
echo "fake adb: no script for: $*" >&2
exit 127
