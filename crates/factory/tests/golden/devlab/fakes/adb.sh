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
    # adb runs its arguments as one command line, whether they come as one or several; each
    # command of a "a; b" line plays on its own. A phone's global settings are world/settings
    # ("serial key value" lines), its screen sleeps while world/asleep names it and wakes on
    # KEYCODE_WAKEUP, and it is on the power world/power names (USB when none). A phone
    # world/slow names takes 6 s to answer whether its lock screen shows.
    shift
    printf '%s\n' "$*" | tr ';' '\n' | while IFS= read -r cmd; do
      case "$cmd" in
      *ro.product.cpu.abi*) case "$serial" in emulator-*) echo x86_64 ;; *) echo arm64-v8a ;; esac ;;
      *sys.boot_completed*) grep -qxF "$serial" "$w/stalled" || echo 1 ;;
      *dumpsys\ window*)
        if grep -qxF "$serial" "$w/slow"; then sleep 6; fi
        grep -qxF "$serial" "$w/locked" && echo "  mDreamingLockscreen=true" ;;
      *dumpsys\ power*)
        if grep -qxF "$serial" "$w/asleep"; then echo "  mWakefulness=Asleep"; else echo "  mWakefulness=Awake"; fi ;;
      *dumpsys\ battery*)
        on=$(awk -v s="$serial" '$1 == s { p = $2 } END { print (p == "" ? "USB" : p) }' "$w/power")
        for p in AC USB Wireless; do
          if [ "$p" = "$on" ]; then echo "  $p powered: true"; else echo "  $p powered: false"; fi
        done ;;
      *settings\ put\ global\ *)
        set -- $cmd
        awk -v s="$serial" -v k="$4" '!($1 == s && $2 == k)' "$w/settings" > "$w/settings.new"
        echo "$serial $4 $5" >> "$w/settings.new"
        mv "$w/settings.new" "$w/settings" ;;
      *settings\ get\ global\ *)
        set -- $cmd
        awk -v s="$serial" -v k="$4" '$1 == s && $2 == k { v = $3 } END { print (v == "" ? "null" : v) }' "$w/settings" ;;
      *KEYCODE_WAKEUP*)
        grep -vxF "$serial" "$w/asleep" > "$w/asleep.new"
        mv "$w/asleep.new" "$w/asleep" ;;
      esac
    done
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
