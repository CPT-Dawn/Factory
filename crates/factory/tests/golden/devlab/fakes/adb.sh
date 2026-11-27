# The world's adb: devices come from world/devices, one "serial state props..." line each.
w='@WORLD@'
row() { awk -v s="$1" '$1 == s' "$w/devices"; }
frame() { awk '{ printf "%s\t%s\n", $1, $2 }' "$w/devices"; }
# A yanked phone is unplugged now, once every running track stream has shown it gone (as the
# harness waits after an unplug); it is yanked once.
yank() {
  grep -qxF "$1" "$w/yanked" || return 1
  grep -vxF "$1" "$w/yanked" > "$w/yanked.new"; mv "$w/yanked.new" "$w/yanked"
  (
    flock 9
    awk -v s="$1" '$1 != s' "$w/devices" > "$w/devices.new"; mv "$w/devices.new" "$w/devices"
  ) 9> "$w/devices.lock"
  n=0
  while [ "$n" -lt 200 ]; do
    now=$(cat "$w/devices") shown=1
    for f in "$w"/tracked.*; do
      [ -e "$f" ] && kill -0 "${f##*.}" 2>/dev/null || continue
      [ "$(cat "$f")" = "$now" ] || shown=
    done
    [ -n "$shown" ] && break
    n=$((n + 1)); sleep 0.05
  done
  echo "adb: device '$1' not found" >&2
}
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
    case "$*" in *ro.product.cpu.abi*) yank "$serial" && exit 1 ;; esac
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
        # The daemon readies a phone as it is plugged in while a grant readies it too: one
        # put at a time, so neither loses the other's.
        set -- $cmd
        (
          flock 9
          awk -v s="$serial" -v k="$4" '!($1 == s && $2 == k)' "$w/settings" > "$w/settings.new"
          echo "$serial $4 $5" >> "$w/settings.new"
          mv "$w/settings.new" "$w/settings"
        ) 9> "$w/settings.lock" ;;
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
    # emu kill: the emulator saves its snapshot and exits, and its unit with it. Adb forgets
    # it first and the unit ends last: whoever sees the unit end (the daemon that stops an
    # emulator waits for that) may start a new emulator on the serial at once, and one line of
    # the world must not take the new one with it. Each file is changed under its lock, as
    # the harness changes it.
    if [ "$2" = kill ]; then
      unit=$(awk -v p="${serial#emulator-}" '$2 == p { print $1 }' "$w/unit-ports" | tail -1)
      (
        flock 9
        awk -v s="$serial" '$1 != s' "$w/devices" > "$w/devices.new"
        mv "$w/devices.new" "$w/devices"
      ) 9> "$w/devices.lock"
      if [ -n "$unit" ]; then
        (
          flock 9
          grep -vxF "$unit" "$w/units" > "$w/units.new"
          mv "$w/units.new" "$w/units"
        ) 9> "$w/units.lock"
      fi
    fi
    exit 0 ;;
  reverse)
    # world/reverses holds "serial device-port host-port" lines, as the device keeps its
    # reverses; `reverse --list` shows a phone's. Setting one prints nothing.
    r="$w/reverses"; touch "$r"
    case "$1:$2" in
    reverse:--remove-all) awk -v s="$serial" '$1 != s' "$r" > "$r.new"; mv "$r.new" "$r" ;;
    reverse:--remove) awk -v s="$serial" -v p="$3" '!($1 == s && $2 == p)' "$r" > "$r.new"; mv "$r.new" "$r" ;;
    reverse:--list) awk -v s="$serial" '$1 == s' "$r" ;;
    *)
      awk -v s="$serial" -v p="$2" '!($1 == s && $2 == p)' "$r" > "$r.new"; mv "$r.new" "$r"
      echo "$serial $2 $3" >> "$r" ;;
    esac
    exit 0 ;;
  esac ;;
esac
echo "fake adb: no script for: $*" >&2
exit 127
