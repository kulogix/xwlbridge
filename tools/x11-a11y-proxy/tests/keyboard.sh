#!/bin/bash
# Keyboard accessibility through the proxy (GTK 3 probe on the engine): sticky keys with and without modifier sync,
# Caps Lock toggled while another window had focus (lock sync), slow keys (mutter filters before the proxy), and
# focus set on a mirror by an assistive technology moving the app's focus (GNOME -> X focus sync).
OUT=${OUT:-/w/out/a11y-proxy/keyboard}; rm -rf "$OUT"; ORCA=${ORCA:-1}; source /w/tools/x11-a11y-proxy/tests/lib.sh
R=$OUT/result.txt
text_of() { AT_SPI_BUS_ADDRESS=$PRIV python3 - "$1" <<'PY'
import sys, gi
gi.require_version('Atspi', '2.0'); from gi.repository import Atspi
d = Atspi.get_desktop(0)
def walk(o, k=0):
    if k > 20: return None
    if o.get_role_name() == 'text': return Atspi.Text.get_text(o, 0, -1)
    for i in range(o.get_child_count()):
        r = walk(o.get_child_at_index(i), k + 1)
        if r is not None: return r
for i in range(d.get_child_count()):
    a = d.get_child_at_index(i)
    for j in range(a.get_child_count()):
        w = a.get_child_at_index(j)
        if w and w.get_name() == sys.argv[1]: print(repr(walk(w))); sys.exit(0)
print('NOT FOUND')
PY
}
# 1. sticky keys: Tab Tab (entry), tap Shift, a, a -> "Aa"
gsettings set org.gnome.desktop.a11y.keyboard enable true
gsettings set org.gnome.desktop.a11y.keyboard stickykeys-enable true
for arm in modsync nomodsync; do
  flags=""; [ $arm = nomodsync ] && flags=--no-modsync
  mark "sticky $arm"; PROXY_FLAGS=$flags proxy_start sticky_$arm python3 $TA/gtk3_probe.py K$arm; sleep 10
  GI key 15 sleep 1 key 15 sleep 1 key 42 sleep 0.8 key 30 sleep 0.6 key 30 sleep 1.5
  echo "sticky keys, proxy $arm: $(text_of "GTK3 probe K$arm")" >>$R
  kill $PROXY; sleep 2
done
gsettings set org.gnome.desktop.a11y.keyboard stickykeys-enable false
# 2. Caps Lock pressed while a native GNOME window had focus, then typing in the app through the proxy
GDK_BACKEND=wayland python3 $T/tests/witness.py >"$OUT/witness.log" 2>&1 & WIT=$!; sleep 4
mark "capslock on in witness"; GI key 58 sleep 1 key 48 sleep 1
# no-modsync first: the reverse keyboard keeps its lock state between proxies (one master per engine)
for arm in nomodsync modsync; do
  flags=""; [ $arm = nomodsync ] && flags=--no-modsync
  mark "caps $arm"; PROXY_FLAGS=$flags proxy_start caps_$arm python3 $TA/gtk3_probe.py C$arm; sleep 10
  GI key 15 sleep 1 key 15 sleep 1 key 48 sleep 1.5
  echo "caps lock on (pressed in another window), typed 'b' through the proxy $arm: $(text_of "GTK3 probe C$arm")" >>$R
  kill $PROXY; sleep 2
done
echo "witness got: $(tail -1 $OUT/witness.log)" >>$R
GI key 58 sleep 1   # caps lock off again (in the proxy's window: forwarded, so both are off)
mark "caps off"; proxy_start caps_off python3 $TA/gtk3_probe.py Coff; sleep 10
GI key 15 sleep 1 key 15 sleep 1 key 48 sleep 1.5
echo "caps lock off again, typed 'b' through the proxy (modsync): $(text_of "GTK3 probe Coff")" >>$R
kill $PROXY; sleep 2
kill $WIT 2>/dev/null
# 2b. Num Lock turned on while a native GNOME window had focus, then keypad 1 typed in the app through the proxy:
#     '1' when the engine's Num Lock follows GNOME's, nothing (KP_End) when it does not
GDK_BACKEND=wayland python3 $T/tests/witness.py >"$OUT/witness_num.log" 2>&1 & WIT=$!; sleep 4
mark "numlock on in witness"; GI key 69 sleep 1
for arm in nomodsync modsync; do
  flags=""; [ $arm = nomodsync ] && flags=--no-modsync
  mark "num $arm"; PROXY_FLAGS=$flags proxy_start num_$arm python3 $TA/gtk3_probe.py N$arm; sleep 10
  GI key 15 sleep 1 key 15 sleep 1 key 79 sleep 1.5
  echo "num lock on (pressed in another window), typed keypad 1 through the proxy $arm: $(text_of "GTK3 probe N$arm")" >>$R
  kill $PROXY; sleep 2
done
mark "numlock off"; GI key 69 sleep 1   # off again: Orca's flat review below needs the keypad's navigation keys
kill $WIT 2>/dev/null
# 3. slow keys (mutter): a short press is dropped, a long one accepted, before the proxy sees anything
gsettings set org.gnome.desktop.a11y.keyboard slowkeys-enable true
gsettings set org.gnome.desktop.a11y.keyboard slowkeys-delay 400
mark "slowkeys"; proxy_start slow python3 $TA/gtk3_probe.py S; sleep 10
# every key held 0.9 s (slow keys accepts it) except the short 'x' (dropped)
GI down 15 sleep 0.9 up 15 sleep 1 down 15 sleep 0.9 up 15 sleep 1.5 down 45 sleep 0.05 up 45 sleep 1 down 46 sleep 0.9 up 46 sleep 1.5
echo "slow keys 400 ms: short 'x' + long 'c' typed through the proxy: $(text_of "GTK3 probe S")" >>$R
kill $PROXY; sleep 2
gsettings set org.gnome.desktop.a11y.keyboard slowkeys-enable false
gsettings set org.gnome.desktop.a11y.keyboard enable false
# 4. an AT moving focus onto a mirror (AT-SPI Component.GrabFocus), and Orca's flat review: next line (KP_9),
#    then "left click on the current flat review item" (KP_Divide): Orca clicks through the compositor, the click
#    reaches the proxy's pointer path and the app
mark "at focus"; proxy_start focus python3 $TA/gtk3_probe.py F; sleep 10
python3 $T/tests/at_do.py x11-a11y-proxy "check box" "Check me" focus >>"$OUT/at_do.log" 2>&1; sleep 1.5
echo "AT GrabFocus on a mirror: $(tail -1 $OUT/at_do.log)" >>$R
mark "flat review next line, left click"; GI key 72 sleep 1.5 key 73 sleep 1.5 key 98 sleep 2
echo "flat review KP_9 + KP_Divide on the next line -> app effects: $(grep -E 'TOGGLE|ACTIVATE' $OUT/app_focus.log | tr '\n' ' ')" >>$R
kill $PROXY; sleep 2
finish
cat $R
echo KEYBOARD_DONE
