# jam — low-latency online jam sessions

Play music together over the internet, in time. One player **hosts** the
session on their machine; the others **join** after a handshake with a room
code. The host mixes everyone and sends each player a personalized mix back.

Ordinary calling apps add 100–500 ms of delay — unplayable. Musicians stay
in rhythm up to roughly 25–40 ms of mouth-to-ear delay (like standing ~10 m
apart on stage). `jam` is engineered end-to-end for that budget:

- 48 kHz audio in **2.5 ms frames**, Opus CELT low-delay codec (~30 bytes/frame)
- raw **UDP** transport with a purpose-built 8-byte header — no TCP, no TLS
  handshakes, no congestion-controlled pacing in the way
- lock-free, allocation-free audio path: jitter buffer, mixer, and codec all
  run inside the sound-card callback
- adaptive jitter buffer (2 frames minimum) with Opus packet-loss
  concealment and optional zero-latency redundancy repair
- expect **~20 ms** mouth-to-ear on a LAN and **~RTT + 25 ms** across the
  internet on wired connections

## Quickstart

Build (needs Rust, a C compiler, and cmake; on Linux also
`libasound2-dev`):

```console
$ cargo build --release
$ ./target/release/jam devices          # check your interface shows up
```

If the build fails on Windows/macOS with a CMake error about
`cmake_minimum_required` compatibility, your CMake is 4.x and the bundled
libopus needs the compatibility switch:

```console
$ CMAKE_POLICY_VERSION_MINIMUM=3.5 cargo build --release        # mac/linux
> $env:CMAKE_POLICY_VERSION_MINIMUM="3.5"; cargo build --release  # powershell
```

**Host** (the player with the best upload bandwidth):

```console
$ jam host
hosting room QX3JD7 on UDP port 47820
  LAN:      jam join 192.168.1.23:47820 --code QX3JD7
  internet: jam join 203.0.113.7:47820 --code QX3JD7   (forward UDP 47820 ...)
```

**Everyone else**:

```console
$ jam join 203.0.113.7:47820 --code QX3JD7 --name maria
```

The status display shows per-player level meters, packet loss, jitter-buffer
depth, RTT, and an estimated mouth-to-ear latency. As the host you run the
mix: select a player with `0-4`, then `+`/`-` adjusts their gain, `m` mutes
them (dropped from everyone's mix; they still hear the room), and `s` solos
them (while anything is soloed, only soloed players are heard). Mute/solo
fade in and out over ~10 ms, so toggling them mid-song doesn't click.

Prefer a window? Build with the GUI and pass `--gui` to `host`/`join` for
an egui session window with faders, meters, and per-player **M**/**S**
buttons:

```console
$ cargo build --release --features gui
$ ./target/release/jam host --gui
```

## Reaching the host (NAT)

Clients connect **to** the host's UDP port, so the host must be reachable.
Three options, in order of preference:

1. **Tailscale / ZeroTier (zero configuration)** — install on every
   machine, use the overlay IP (`100.x.y.z` for Tailscale) as the join
   address. No router changes, adds ~1–3 ms. This is the easiest path and
   works from cafés, dorms, CGNAT connections, everywhere.
2. **Port forwarding** — forward **UDP 47820** on the host's router to the
   host machine, then share the public address `jam host` prints (it
   discovers it via STUN). Standard "hosting a game server" setup.
3. **LAN** — same network: just use the LAN address that `jam host` prints.

Verify reachability without touching audio:

```console
$ jam ping 203.0.113.7:47820
  reply 1: rtt 18.42 ms
  ...
network contribution to mouth-to-ear latency ≈ rtt (18.4 ms)
```

A rendezvous/hole-punching server (no port forwarding, no VPN) is on the
roadmap; the protocol reserves room for it.

## Getting the latency down

The budget, roughly: `capture buffer + 2.5 ms framing + network + jitter
buffers + playback buffer`, plus the codec's 2.5 ms lookahead and your
interface's converter latency.

- **Wired ethernet, always.** WiFi adds jitter the buffer must absorb; every
  ms of jitter costs ~3 ms of buffer.
- **Small hardware buffers**: `--buffer 64` if your interface/driver holds up
  (watch the `xruns` counter; the default is 128).
- **Windows**: stock WASAPI shared mode has a ~10 ms floor. Build with
  `--features asio` (needs the Steinberg ASIO SDK, or let `asio-sys`
  download it) and use an ASIO driver for 64–128-sample buffers.
- **Linux**: use JACK/PipeWire (`--features jack`); force a small quantum
  with `PIPEWIRE_LATENCY=128/48000`.
- **Monitoring**: the default `--monitor direct` mixes your own signal into
  your output locally (instantly) and the host sends you everyone-but-you.
  If your interface has hardware direct monitoring, use that instead
  (`--monitor off`) — it's 0 ms. `--monitor mix` sends your own signal
  through the host like everyone else's (some ensembles prefer sharing one
  "room time"; Jamulus works this way by default).
- **Rough connections**: `--redundancy` doubles audio bandwidth and repairs
  any single packet loss with zero added latency; `--frame 5` halves packet
  rate at +2.5 ms; the jitter buffer adapts on its own (fix it for testing
  with `--jitter <frames>`).
- **LAN sessions**: `--pcm` skips the codec entirely (~1 Mbit/s per stream).

### Measuring, not guessing

- `jam ping <host>` — network RTT through the actual UDP path.
- `jam measure --host <addr> --code <code>` against a host started with
  `--echo` — measures the full network + codec + jitter-buffer round trip
  (no sound card needed).
- `jam measure --loopback` with a physical cable from your interface's
  output to its input — measures your local device round trip.
- Mouth-to-ear ≈ echo measurement + loopback measurement. The status
  display's estimate cross-checks against these.

> **All players must use the same `--frame` and codec (`--pcm` or not).** The
> host fixes these for the session; if a client's flags disagree, it joins but
> the status line shows a "parameter mismatch" warning and audio stays silent
> until you restart with matching flags. (Automatic reconfiguration from the
> host's WELCOME is on the roadmap.)

## Security model (MVP)

Traffic is **unencrypted** UDP; the room code (sent in the handshake) gates
entry, source addresses are pinned per player, session ids scope every
packet, and BYE/audio are dropped unless they carry the right session id.
Treat sessions like a phone call on an untrusted network: fine for jamming,
not for secrets. Encrypted transport (DTLS/QUIC) is a planned upgrade; the
version field in every packet makes the migration non-breaking.

Known residual exposure for an attacker who can both guess the (public) room
code *and* forge UDP source addresses on your path: the diagnostic `PING`
echo is answered without a session check (a minor ~2× reflection vector), and
the 16-bit session id is brute-forceable in the worst case. These are
acceptable for the "jamming with friends" threat model and close fully once
encrypted transport lands. Don't expose a host to the open internet you
wouldn't also expose a game server to.

## How it works

```
crates/jam-protocol   wire format, seq arithmetic, handshake state machines
                      (pure, no I/O — property-tested)
crates/jam-audio      cpal devices, Opus wrapper, lock-free jitter buffer,
                      mixer/soft-clip, meters, drift resampler
crates/jam-core       host/client pipelines, UDP RX/TX threads, control
                      thread (keepalives, RTT, roster, timeouts), stats
crates/jam-app        the `jam` binary: CLI, status UI, STUN, measurement
```

Per participant: the **audio callback** does capture → Opus encode → packet
(and jitter-buffer pop → decode → mix → playback) with no locks, allocation,
or syscalls; a **receive thread** parses datagrams into per-sender jitter
buffers; a **transmit thread** drains a lock-free ring; a **control thread**
runs the handshake/keepalive state machines. The host additionally decodes
every client, mixes with per-player smoothed gains, and returns
**mix-minus-self** to each player.

Packet loss → Opus PLC conceals a 2.5 ms gap (inaudible in the common
case); `--redundancy` piggybacks the previous frame on every packet.
Clock drift between sound cards → a PI-controlled cubic-Hermite fractional
resampler continuously rate-matches each incoming stream (soak-tested
against ±200 ppm offsets); coarse frame drop/insert at quiet moments
remains as a fallback for step changes, and `--no-drift` disables the
resampler entirely.

## Development

```console
$ cargo test --workspace        # protocol, DSP, and localhost session tests
$ cargo clippy --workspace --all-targets
$ scripts/netem.sh start        # simulate 20ms ±5ms jitter + 2% loss on lo
$ scripts/netem.sh stop
```

The end-to-end tests (`crates/jam-core/tests/localhost.rs`) run a real host
and clients over localhost UDP with the audio pipelines driven headlessly —
handshake, mixing, mix-minus-self, and loss repair are verified spectrally
without any sound hardware. Under netem you can watch the adaptive buffer
react: run the tests or a real session on `lo` with impairment on.

There is also a hidden `--simulate-loss <pct>` flag on `host`/`join` for
loss testing on platforms without netem.

## Roadmap

- rendezvous server + UDP hole punching (skip port forwarding)
- percentile-based (NetEQ-style) jitter estimator for bursty WiFi links
- stereo mixes, encrypted transport
