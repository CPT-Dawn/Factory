# Desktop notifications go nowhere; each records the session bus it was given in world/notify-bus.
printf '%s\n' "${DBUS_SESSION_BUS_ADDRESS-unset}" >> '@WORLD@/notify-bus'
exit 0
