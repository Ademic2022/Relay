//! `relay-server --rendezvous --relay` (§8.3). Networking lands in M3.

use std::process::ExitCode;

#[derive(Debug, Default, PartialEq, Eq)]
struct Args {
    rendezvous: bool,
    relay: bool,
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut a = Args::default();
    for arg in args {
        match arg.as_str() {
            "--rendezvous" => a.rendezvous = true,
            "--relay" => a.relay = true,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if !a.rendezvous && !a.relay {
        a.rendezvous = true;
        a.relay = true;
    }
    Ok(a)
}

fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{e}");
            }
            eprintln!("usage: relay-server [--rendezvous] [--relay]");
            return ExitCode::from(2);
        }
    };
    if args.rendezvous {
        println!(
            "rendezvous: would listen on UDP :{}",
            relay_net::RENDEZVOUS_PORT
        );
    }
    if args.relay {
        println!("relay: would listen on UDP :{}", relay_net::RELAY_PORT);
    }
    eprintln!("relay-server: networking not implemented yet (milestone M3)");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_both() {
        assert_eq!(
            parse(Vec::<String>::new()).unwrap(),
            Args {
                rendezvous: true,
                relay: true
            }
        );
        assert_eq!(
            parse(["--relay".to_string()]).unwrap(),
            Args {
                rendezvous: false,
                relay: true
            }
        );
        assert!(parse(["--bogus".to_string()]).is_err());
    }
}
