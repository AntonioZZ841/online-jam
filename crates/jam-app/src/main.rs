mod cli;
mod measure;
mod status_ui;
mod stun;

use anyhow::{bail, Context};
use clap::Parser;
use cli::{Cli, Command, HostArgs, JoinArgs, MeasureArgs, PingArgs};
use jam_core::engine::{start_client, start_host, AudioConfig, ClientConfig, HostConfig};
use jam_protocol::packet::{parse_datagram, Control, Header, PacketType, Payload};
use jam_protocol::{DEFAULT_PORT, UNJOINED_SENDER_ID};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command {
        Command::Devices => devices(),
        Command::Host(args) => host(args),
        Command::Join(args) => join(args),
        Command::Ping(args) => ping(args),
        Command::Measure(args) => run_measure(args),
    }
}

fn devices() -> anyhow::Result<()> {
    let list = jam_audio::device::list_devices().context("enumerating audio devices")?;
    if list.is_empty() {
        println!("no audio devices found");
        return Ok(());
    }
    println!("{:<40} {:>6} {:>7}", "device", "input", "output");
    for d in list {
        let mark = |b: bool, def: bool| match (b, def) {
            (true, true) => "yes*",
            (true, false) => "yes",
            _ => "-",
        };
        println!(
            "{:<40} {:>6} {:>7}",
            d.name,
            mark(d.is_input, d.default_input),
            mark(d.is_output, d.default_output),
        );
    }
    println!("\n(* = system default; pick with --input/--output <name substring>)");
    Ok(())
}

fn resolve(host: &str) -> anyhow::Result<SocketAddr> {
    let with_port = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:{DEFAULT_PORT}")
    };
    with_port
        .to_socket_addrs()
        .with_context(|| format!("resolving {host}"))?
        .find(|a| a.is_ipv4())
        .with_context(|| format!("no IPv4 address for {host}"))
}

fn host(args: HostArgs) -> anyhow::Result<()> {
    let code = cli::room_code(args.code.as_deref())?;
    let params = args.audio.params(args.echo)?;

    // STUN before the session claims the port, so the reply is not swallowed
    // by the session's receive thread. The NAT mapping usually survives the
    // rebind since local port stays the same.
    let public = if args.no_stun {
        None
    } else {
        UdpSocket::bind(("0.0.0.0", args.port))
            .ok()
            .and_then(|s| stun::discover_public_addr(&s))
    };

    let (handle, shared) = start_host(HostConfig {
        port: args.port,
        room_code: code,
        name: args.name.clone(),
        params,
        audio: AudioConfig {
            input_name: args.audio.input.clone(),
            output_name: args.audio.output.clone(),
            hw_buffer: args.audio.buffer,
        },
    })
    .context("starting host session")?;

    let code_str = String::from_utf8_lossy(&code).into_owned();
    println!("hosting room {code_str} on UDP port {}", args.port);
    if let Some(ip) = stun::lan_addr() {
        println!("  LAN:      jam join {ip}:{} --code {code_str}", args.port);
    }
    match public {
        Some(addr) => println!(
            "  internet: jam join {addr} --code {code_str}   (forward UDP {} on your router,\n            or skip forwarding entirely by using Tailscale/ZeroTier addresses)",
            args.port
        ),
        None => println!(
            "  internet: public address lookup failed — forward UDP {} and share your IP,\n            or use Tailscale/ZeroTier (see README)",
            args.port
        ),
    }
    if args.echo {
        println!("  echo mode: each player hears their own audio looped back (measurement)");
    }
    println!("\npress any key to open the session display...");
    let _ = std::io::stdin().read_line(&mut String::new());

    status_ui::run(
        &handle.snapshot,
        &status_ui::UiShared::Host(shared),
        &handle.xruns,
    )?;
    handle.shutdown();
    Ok(())
}

fn join(args: JoinArgs) -> anyhow::Result<()> {
    let addr = resolve(&args.host)?;
    let code = cli::room_code(Some(&args.code))?;
    let params = args.audio.params(false)?;
    let (handle, shared) = start_client(ClientConfig {
        host_addr: addr,
        room_code: code,
        name: args.name.clone(),
        params,
        audio: AudioConfig {
            input_name: args.audio.input.clone(),
            output_name: args.audio.output.clone(),
            hw_buffer: args.audio.buffer,
        },
    })
    .context("starting client session")?;

    status_ui::run(
        &handle.snapshot,
        &status_ui::UiShared::Client(shared),
        &handle.xruns,
    )?;
    handle.shutdown();
    Ok(())
}

fn ping(args: PingArgs) -> anyhow::Result<()> {
    let addr = resolve(&args.host)?;
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    let epoch = Instant::now();
    let mut rtts = vec![];
    let mut buf = [0u8; 256];
    println!("pinging {addr} with {} probes...", args.count);
    for i in 0..args.count {
        let t1 = epoch.elapsed().as_micros() as u64;
        let ping = Control::Ping { t1_us: t1 };
        let header = Header {
            ptype: PacketType::Ping,
            session_id: 0,
            sender_id: UNJOINED_SENDER_ID,
            seq: i as u16,
        };
        let mut out = [0u8; 64];
        let n = ping.write(&header, &mut out);
        socket.send_to(&out[..n], addr)?;
        match socket.recv_from(&mut buf) {
            Ok((len, from)) if from == addr => {
                let t4 = epoch.elapsed().as_micros() as u64;
                if let Ok((
                    _,
                    Payload::Control(Control::Pong {
                        t1_us,
                        t2_us,
                        t3_us,
                    }),
                )) = parse_datagram(&buf[..len])
                {
                    let rtt_us = t4
                        .saturating_sub(t1_us)
                        .saturating_sub(t3_us.saturating_sub(t2_us));
                    let rtt_ms = rtt_us as f64 / 1_000.0;
                    println!("  reply {}: rtt {rtt_ms:.2} ms", i + 1);
                    rtts.push(rtt_ms);
                }
            }
            _ => println!("  reply {}: timeout", i + 1),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if rtts.is_empty() {
        bail!("no replies — host unreachable (check address, port forwarding, firewall)");
    }
    let min = rtts.iter().cloned().fold(f64::MAX, f64::min);
    let max = rtts.iter().cloned().fold(0.0, f64::max);
    let avg = rtts.iter().sum::<f64>() / rtts.len() as f64;
    println!(
        "\n{} replies: rtt min {min:.2} / avg {avg:.2} / max {max:.2} ms",
        rtts.len()
    );
    println!("network contribution to mouth-to-ear latency ≈ rtt ({avg:.1} ms)");
    Ok(())
}

fn run_measure(args: MeasureArgs) -> anyhow::Result<()> {
    if let Some(host) = &args.host {
        let addr = resolve(host)?;
        let code = cli::room_code(args.code.as_deref())
            .context("--host measurement needs --code from the echo host")?;
        let params = args.audio.params(false)?;
        let result = measure::net_measure(addr, code, params)?;
        result.print("network + processing round trip");
        println!("(add your local device round trip from `jam measure --loopback`)");
        Ok(())
    } else if args.loopback {
        let result = measure::loopback_measure(
            args.audio.input.as_deref(),
            args.audio.output.as_deref(),
            args.audio.buffer,
        )?;
        result.print("device round trip (out -> cable -> in)");
        Ok(())
    } else {
        bail!("choose --loopback (cable on the local interface) or --host <addr> --code <code> (against a `jam host --echo`)");
    }
}
