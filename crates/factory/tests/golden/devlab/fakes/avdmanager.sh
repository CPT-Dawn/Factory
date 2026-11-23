# The world's avdmanager: "create avd -n NAME" writes NAME's config.ini under ~/.android/avd.
name=
prev=
for a in "$@"; do
  [ "$prev" = -n ] && name=$a
  prev=$a
done
mkdir -p "$HOME/.android/avd/$name.avd"
printf 'avd.ini.encoding=UTF-8\nhw.ramSize=1536\n' > "$HOME/.android/avd/$name.avd/config.ini"
