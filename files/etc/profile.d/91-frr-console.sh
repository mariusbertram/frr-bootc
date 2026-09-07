# Launch the FRR status dashboard (/usr/local/bin/frr-console) on
# interactive console logins - serial (virtctl console), graphical/VNC
# (virtctl vnc) and SSH alike, since all three just land in a normal
# interactive login shell and this only checks for that, never for a
# specific tty/console type.
#
# Skipped for non-interactive/non-tty sessions (scp/rsync/ansible/CI over
# ssh run the shell with `-c command`, which never sources /etc/profile in
# the first place, but the checks below are kept as a defensive
# belt-and-braces in case that ever changes), when frr-console itself
# isn't present/executable, when disabled via /etc/frr-console.disabled,
# and - via FRR_CONSOLE_ACTIVE - when this is the bash the dashboard's own
# 'b' key spawned, so pressing 'b' doesn't relaunch the dashboard inside
# itself.
#
# `exec`, not a plain call: the dashboard IS the login shell here, not
# something that runs before it - quitting it ('q') ends the session, the
# same as 'exit' at a plain shell prompt would.
case $- in
    *i*) ;;
    *) return ;;
esac
[ -t 0 ] && [ -t 1 ] || return
[ -n "${FRR_CONSOLE_ACTIVE:-}" ] && return
[ -e /etc/frr-console.disabled ] && return
[ -x /usr/local/bin/frr-console ] || return

exec /usr/local/bin/frr-console
