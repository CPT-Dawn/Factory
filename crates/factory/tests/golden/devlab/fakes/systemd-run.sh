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
echo "$unit" >> "$w/units"
echo "emulator-$port device product:sdk_gphone64_x86_64 model:sdk_gphone64_x86_64 device:emu64xa" >> "$w/devices"
