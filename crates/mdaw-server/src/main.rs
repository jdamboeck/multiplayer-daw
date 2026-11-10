use anyhow::{Context, Result, bail};
use mdaw_server::{Config, Server};
use std::net::SocketAddr;
use std::path::PathBuf;

const USAGE: &str = "\
mdaw-server: serves one project to mdaw clients.

Usage: mdaw-server [--data DIR] [--listen ADDR:PORT] [--host NAME]

  --data DIR          where the project is kept (default: ./mdaw-data)
  --listen ADDR:PORT  UDP address to listen on (default: 0.0.0.0:7464)
  --host NAME         name or address clients reach this machine by, for the join string

On start it prints a join string. Pass it to a client: mdaw --join 'mdaw://…'";

fn main() -> Result<()> {
    let mut config = Config {
        data_dir: PathBuf::from("mdaw-data"),
        listen: SocketAddr::from(([0, 0, 0, 0], 7464)),
        host: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().with_context(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--data" => config.data_dir = value("--data")?.into(),
            "--listen" => {
                let addr = value("--listen")?;
                config.listen = addr
                    .parse()
                    .with_context(|| format!("{addr:?} is not an address like 0.0.0.0:7464"))?;
            }
            "--host" => config.host = Some(value("--host")?),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }

    let local_only = config.host.is_none() && config.listen.ip().is_unspecified();
    let server = Server::start(config)?;
    eprintln!("listening on {}", server.local_addr()?);
    if local_only {
        eprintln!("the join string below works on this machine; pass --host for one that works elsewhere");
    }
    // The join string alone goes to stdout, so scripts can read it.
    println!("{}", server.join_info());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(tokio::signal::ctrl_c())?;
    eprintln!("stopping");
    server.stop();
    Ok(())
}
