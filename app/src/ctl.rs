//! Controlling the mixer from the command line: `LemonMix.exe --control ...`
//! and `LemonMixCtl.exe ...`, for Stream Deck "open" actions, AutoHotkey,
//! scripts and terminals. Each command is one request over the control pipe.

use std::io::{BufReader, Write};

use crate::pipe_client::{CONTROL_PIPE, call_pipe, open_pipe, read_line, send};
use crate::protocol::{BusView, ControlAction, MixerCommand, Request, Snapshot, StripView};

pub const USAGE: &str = "\
Controls a running Lemon Mix.

Commands:
  status                              List the strips and buses
  mute   <target> [on|off|toggle]     Mute a strip or bus
  solo   <target> [on|off|toggle]     Solo a strip
  mono   <target> [on|off|toggle]     Sum a strip's or bus's left and right
  route  <strip> <bus> [on|off|toggle]
                                      Send a strip to a bus
  gain   <target> <dB>                Set a fader, -60 to 12 dB
  adjust <target> <dB>                Move a fader up (3) or down (-3)
  profile <name>                      Switch to a profile
  watch  [meters]                     Print events as the mixer changes, until Ctrl+C
  json   <request>                    Send a request as JSON (see docs/control-protocol.md)

A target is a strip's or bus's name (\"Mic\"), its ID (H1, A2, V3, B3), its key
(from status), or strip:<name> or bus:<name> where a strip and bus share a
name. A switch without on or off is toggled.

Examples:
  LemonMix.exe --control mute Mic
  LemonMix.exe --control route Mic A2 on
  LemonMix.exe --control adjust \"Game audio\" -3";

/// What a command line asks for.
#[derive(Debug, PartialEq)]
pub enum Command {
    Status,
    Send(Request),
    Watch { meters: bool },
    Help,
}

/// Runs a command line and returns the exit code: 0 done, 1 refused or
/// failed, 2 not understood.
pub fn run(args: &[String]) -> i32 {
    let command = match parse(args) {
        Ok(command) => command,
        Err(err) => {
            eprintln!("{err}\n\nRun with help for the commands.");
            return 2;
        }
    };
    let result = match command {
        Command::Help => {
            println!("{USAGE}");
            Ok(())
        }
        Command::Status => call_pipe(CONTROL_PIPE, &Request::Status).and_then(|reply| {
            reply.result()?;
            print!("{}", format_status(&reply.snapshot));
            Ok(())
        }),
        Command::Send(request) => call_pipe(CONTROL_PIPE, &request).and_then(|reply| reply.result()),
        Command::Watch { meters } => watch(meters),
    };
    let _ = std::io::stdout().flush();
    match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("{err}");
            1
        }
    }
}

/// Reads a command line.
pub fn parse(args: &[String]) -> Result<Command, String> {
    let Some((verb, rest)) = args.split_first() else {
        return Ok(Command::Help);
    };
    let control = |action| Ok(Command::Send(Request::Control { action }));
    let switch = |word: Option<&String>| -> Result<Option<bool>, String> {
        match word.map(|w| w.to_ascii_lowercase()).as_deref() {
            None | Some("toggle") => Ok(None),
            Some("on" | "true" | "1") => Ok(Some(true)),
            Some("off" | "false" | "0") => Ok(Some(false)),
            Some(other) => Err(format!("Expected on, off or toggle, not \"{other}\".")),
        }
    };
    let db = |word: &String| -> Result<f32, String> {
        word.trim_end_matches("dB")
            .trim_end_matches("db")
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| format!("Expected a number of dB, not \"{word}\"."))
    };
    let arity = |min: usize, max: usize| -> Result<(), String> {
        if (min..=max).contains(&rest.len()) { Ok(()) } else { Err(format!("Wrong number of arguments for {verb}.")) }
    };
    match verb.to_ascii_lowercase().as_str() {
        "help" | "-h" | "--help" | "/?" => Ok(Command::Help),
        "status" => arity(0, 0).map(|()| Command::Status),
        "mute" | "solo" | "mono" => {
            arity(1, 2)?;
            let (target, on) = (rest[0].clone(), switch(rest.get(1))?);
            control(match verb.to_ascii_lowercase().as_str() {
                "mute" => ControlAction::Mute { target, on },
                "solo" => ControlAction::Solo { target, on },
                _ => ControlAction::Mono { target, on },
            })
        }
        "route" => {
            arity(2, 3)?;
            control(ControlAction::Route { strip: rest[0].clone(), bus: rest[1].clone(), on: switch(rest.get(2))? })
        }
        "gain" => {
            arity(2, 2)?;
            control(ControlAction::Gain { target: rest[0].clone(), db: db(&rest[1])? })
        }
        "adjust" => {
            arity(2, 2)?;
            control(ControlAction::AdjustGain { target: rest[0].clone(), db: db(&rest[1])? })
        }
        "watch" => match rest {
            [] => Ok(Command::Watch { meters: false }),
            [word] if word.eq_ignore_ascii_case("meters") => Ok(Command::Watch { meters: true }),
            _ => Err("Expected watch or watch meters.".into()),
        },
        "profile" => {
            arity(1, usize::MAX)?;
            Ok(Command::Send(Request::Mixer { command: MixerCommand::SwitchProfile { name: rest.join(" ") } }))
        }
        "json" => {
            arity(1, usize::MAX)?;
            let text = rest.join(" ");
            let request: Request = serde_json::from_str(&text).map_err(|err| format!("Not a valid request: {err}"))?;
            if matches!(request, Request::Subscribe { .. }) {
                return Err("Use watch to subscribe.".into());
            }
            Ok(Command::Send(request))
        }
        other => Err(format!("Unknown command \"{other}\".")),
    }
}

/// Prints a subscription's events, one JSON line each, until it ends.
fn watch(meters: bool) -> Result<(), String> {
    let pipe = open_pipe(CONTROL_PIPE).map_err(|_| "Lemon Mix is not running.".to_string())?;
    send(&pipe, &Request::Subscribe { meters })?;
    let mut reader = BufReader::new(&pipe);
    let mut out = std::io::stdout();
    loop {
        let line = read_line(&mut reader).map_err(|_| "Lemon Mix stopped.".to_string())?;
        if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
            return Ok(());
        }
    }
}

/// The strips and buses as a table.
pub fn format_status(snapshot: &Snapshot) -> String {
    let mut text = String::new();
    let gain = |db: f32| if db <= engine::params::MIN_GAIN_DB { "-inf dB".to_string() } else { format!("{db:+.1} dB") };
    let flags = |list: &[(bool, &str)]| {
        let set: Vec<&str> = list.iter().filter(|(on, _)| *on).map(|(_, name)| *name).collect();
        if set.is_empty() { "-".to_string() } else { set.join(",") }
    };
    let name_width = snapshot
        .strips
        .iter()
        .map(|s| s.label.chars().count())
        .chain(snapshot.buses.iter().map(|b| b.label.chars().count()))
        .max()
        .unwrap_or(4)
        .clamp(4, 24);
    if !snapshot.profile.is_empty() {
        text.push_str(&format!("Profile: {}\n", snapshot.profile));
    }
    text.push_str("INPUTS\n");
    text.push_str(&format!(
        "  {:<5} {:<name_width$} {:>9}  {:<14} {:<16} KEY\n",
        "ID", "NAME", "GAIN", "SWITCHES", "SENDS TO"
    ));
    for StripView { id, key, label, params, routes, duck_by, .. } in &snapshot.strips {
        let switches = flags(&[
            (params.mute, "mute"),
            (params.solo, "solo"),
            (params.mono, "mono"),
            (!duck_by.is_empty(), "duck"),
        ]);
        let sends = if routes.is_empty() { "-".to_string() } else { routes.join(",") };
        text.push_str(&format!(
            "  {id:<5} {:<name_width$} {:>9}  {switches:<14} {sends:<16} {key}\n",
            ellipsize(label, name_width),
            gain(params.gain_db)
        ));
    }
    text.push_str("OUTPUTS\n");
    text.push_str(&format!(
        "  {:<5} {:<name_width$} {:>9}  {:<14} {:<16} KEY\n",
        "ID", "NAME", "GAIN", "SWITCHES", "DEVICE"
    ));
    for BusView { id, key, label, params, port, .. } in &snapshot.buses {
        let switches = flags(&[(params.mute, "mute"), (params.mono, "mono"), (params.limiter.is_some(), "limiter")]);
        let device = match port {
            Some(port) if !port.device_name.is_empty() => port.device_name.clone(),
            Some(port) if port.endpoint.is_empty() => "none".to_string(),
            Some(port) => port.state.clone(),
            None => "virtual".to_string(),
        };
        text.push_str(&format!(
            "  {id:<5} {:<name_width$} {:>9}  {switches:<14} {:<16} {key}\n",
            ellipsize(label, name_width),
            gain(params.gain_db),
            ellipsize(&device, 16)
        ));
    }
    text
}

fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}~", text.chars().take(max - 1).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{BusParams, PortView, StripParams};

    fn args(line: &str) -> Vec<String> {
        line.split(' ').map(String::from).collect()
    }

    fn action(line: &str) -> ControlAction {
        match parse(&args(line)) {
            Ok(Command::Send(Request::Control { action })) => action,
            other => panic!("{line}: {other:?}"),
        }
    }

    #[test]
    fn command_lines_become_requests() {
        assert_eq!(action("mute Mic"), ControlAction::Mute { target: "Mic".into(), on: None });
        assert_eq!(action("MUTE A1 off"), ControlAction::Mute { target: "A1".into(), on: Some(false) });
        assert_eq!(action("solo strip:Game on"), ControlAction::Solo { target: "strip:Game".into(), on: Some(true) });
        assert_eq!(action("mono B1 toggle"), ControlAction::Mono { target: "B1".into(), on: None });
        assert_eq!(
            action("route Mic A2 on"),
            ControlAction::Route { strip: "Mic".into(), bus: "A2".into(), on: Some(true) }
        );
        assert_eq!(action("gain Mic -12.5"), ControlAction::Gain { target: "Mic".into(), db: -12.5 });
        assert_eq!(action("adjust Mic +3dB"), ControlAction::AdjustGain { target: "Mic".into(), db: 3.0 });
        assert_eq!(parse(&args("status")), Ok(Command::Status));
        assert_eq!(parse(&args("watch meters")), Ok(Command::Watch { meters: true }));
        assert_eq!(
            parse(&args("profile Gaming night")),
            Ok(Command::Send(Request::Mixer { command: MixerCommand::SwitchProfile { name: "Gaming night".into() } }))
        );
        assert_eq!(parse(&[]), Ok(Command::Help));
        assert_eq!(parse(&["json".into(), "{\"op\":\"status\"}".into()]), Ok(Command::Send(Request::Status)));
        for bad in
            ["mute", "mute Mic maybe", "gain Mic loud", "route Mic", "fly away", "status now", "json {", "gain Mic NaN"]
        {
            assert!(parse(&args(bad)).is_err(), "{bad}");
        }
        assert!(parse(&["json".into(), "{\"op\":\"subscribe\"}".into()]).is_err());
    }

    #[test]
    fn status_lists_strips_and_buses() {
        let snapshot = Snapshot {
            strips: vec![StripView {
                id: "H1".into(),
                key: "a1b2c3d4e5f6".into(),
                label: "Mic".into(),
                params: StripParams { gain_db: -6.0, mute: true, ..Default::default() },
                routes: vec!["A1".into(), "B2".into()],
                duck_by: Vec::new(),
                meter: Default::default(),
                port: Some(PortView::default()),
            }],
            buses: vec![BusView {
                id: "A1".into(),
                key: "0f0f0f0f0f0f".into(),
                label: "A1".into(),
                params: BusParams { gain_db: -60.0, ..Default::default() },
                meter: Default::default(),
                port: Some(PortView { device_name: "Speakers".into(), ..Default::default() }),
            }],
            ..Default::default()
        };
        let text = format_status(&snapshot);
        assert!(text.contains("H1    Mic    -6.0 dB  mute           A1,B2            a1b2c3d4e5f6"), "{text}");
        assert!(text.contains("A1    A1     -inf dB  -              Speakers         0f0f0f0f0f0f"), "{text}");
    }
}
