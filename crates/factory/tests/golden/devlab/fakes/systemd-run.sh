# The world's systemd-run: starting an emulator unit makes it active and its emulator boot at once.
w='@WORLD@'
unit=
port=
prev=
for a in "$@"; do
  case "$a" in --unit=*) unit=${a#--unit=} ;; esac
  [ "$prev" = -port ] && port=$a
  prev=$a
done
if grep -qxF systemd-run "$w/broken"; then
  echo "Failed to start transient service unit: Unit $unit.service was already loaded or has a fragment file." >&2
  exit 1
fi
# Each world file changes under its lock, as the harness and the fake adb change them: the unit
# and its port are one step under the units lock.
(
  flock 9
  echo "$unit" >> "$w/units"
  echo "$unit $port" >> "$w/unit-ports"
) 9> "$w/units.lock"
(
  flock 9
  echo "emulator-$port device product:sdk_gphone64_x86_64 model:sdk_gphone64_x86_64 device:emu64xa" >> "$w/devices"
) 9> "$w/devices.lock"
