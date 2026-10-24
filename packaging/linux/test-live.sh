#!/bin/sh
# The tests that need a real sound server and session bus, in the container
# from this folder (see the Dockerfile):
#
#   docker run --rm -v "$PWD":/src -v sonus-target:/target sonus-linux packaging/linux/test-live.sh
#
# PulseAudio refuses to run as root, so this starts one as a throwaway user,
# with two outputs, two apps playing a tone, and a stand-in Spotify on D-Bus.
set -e
if [ "$(id -u)" = 0 ]; then
  id sonus >/dev/null 2>&1 || useradd -m sonus
  chmod -R a+rwX "${CARGO_TARGET_DIR:-/target}" "$CARGO_HOME" "$RUSTUP_HOME"
  exec su sonus -s /bin/sh -c "PATH=$PATH CARGO_HOME=$CARGO_HOME RUSTUP_HOME=$RUSTUP_HOME CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/target} $0"
fi
cd /src
# Built first: a build longer than the tone would leave nothing playing.
cargo test --no-run --quiet

export XDG_RUNTIME_DIR="$(mktemp -d)"
eval "$(dbus-launch --sh-syntax)"
pulseaudio -D --exit-idle-time=-1 2>/dev/null
sleep 1
pactl load-module module-null-sink sink_name=speakers sink_properties=device.description=Speakers >/dev/null
pactl load-module module-null-sink sink_name=headset sink_properties=device.description=Headset >/dev/null
pactl set-default-sink speakers
python3 -c "
import math, struct, wave
w = wave.open('$XDG_RUNTIME_DIR/tone.wav', 'wb'); w.setnchannels(2); w.setsampwidth(2); w.setframerate(44100)
w.writeframes(b''.join(struct.pack('<hh', *[int(8000 * math.sin(i / 20))] * 2) for i in range(44100 * 30)))"
for app in spotify:spotify firefox:Firefox; do
  paplay --property=application.process.binary="${app%%:*}" \
    --property=application.name="${app#*:}" "$XDG_RUNTIME_DIR/tone.wav" &
done
python3 packaging/linux/fake_player.py >/dev/null 2>&1 &
sleep 1
cargo test --quiet -- --include-ignored
