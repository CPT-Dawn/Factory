# The world's systemctl --user: a unit is active while its name is a line of world/units.
w='@WORLD@'
if [ "$1" = --user ]; then
  case "$2" in
  is-active)
    if [ "$3" = --quiet ]; then grep -qxF "$4" "$w/units"; exit $(( $? ? 3 : 0 )); fi
    if grep -qxF "$3" "$w/units"; then echo active; exit 0; fi
    echo inactive
    exit 3 ;;
  reset-failed)
    exit 0 ;;
  stop)
    # The unit's emulator ends with it: adb no longer lists it.
    # Adb forgets it first and the unit ends last, as with `adb emu kill`.
    port=$(awk -v u="$3" '$1 == u { print $2 }' "$w/unit-ports" | tail -1)
    if [ -n "$port" ]; then
      (
        flock 9
        awk -v s="emulator-$port" '$1 != s' "$w/devices" > "$w/devices.new"
        mv "$w/devices.new" "$w/devices"
      ) 9> "$w/devices.lock"
    fi
    (
      flock 9
      grep -vxF "$3" "$w/units" > "$w/units.new"
      mv "$w/units.new" "$w/units"
    ) 9> "$w/units.lock"
    exit 0 ;;
  esac
fi
echo "fake systemctl: no script for: $*" >&2
exit 127
