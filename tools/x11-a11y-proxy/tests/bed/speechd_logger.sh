# Source: configure Speech Dispatcher (autospawned by Orca) to send every utterance to a generic module whose
# "synthesizer" appends "<time> <text>" to $SPEECH_LOG instead of playing audio. Ground truth for "what reaches speech".
# Speech Dispatcher 0.12 refuses a module whose audio cannot be opened ("server audio is not supported"), so this also
# starts pipewire-pulse with a null sink (verified with Speech Dispatcher 0.12).
SPEECH_LOG=${SPEECH_LOG:-/tmp/speech.log}
pgrep -x pipewire >/dev/null || { pipewire >/tmp/pw.log 2>&1 & sleep 0.5; }
pgrep -x wireplumber >/dev/null || { wireplumber >/tmp/wp.log 2>&1 & sleep 0.5; }
pgrep -x pipewire-pulse >/dev/null || { pipewire-pulse >/tmp/pwp.log 2>&1 & sleep 1; }
pactl load-module module-null-sink sink_name=void >/dev/null 2>&1; pactl set-default-sink void 2>/dev/null
mkdir -p $HOME/.config/speech-dispatcher/modules $HOME/sdlog
cat > $HOME/.config/speech-dispatcher/speechd.conf <<CONF
AddModule "logger" "sd_generic" "logger.conf"
DefaultModule logger
AudioOutputMethod "pulse"
LogLevel 3
LogDir "$HOME/sdlog"
CONF
cat > $HOME/.config/speech-dispatcher/modules/logger.conf <<CONF
GenericExecuteSynth "printf '%s %s\\\\n' \"\\\$(date +%T.%N)\" \\'\$DATA\\' >> $SPEECH_LOG"
GenericCmdDependency "printf"
AddVoice "en" "MALE1" "en"
DefaultVoice "en"
CONF
