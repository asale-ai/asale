//! asaled — the asale client daemon binary.
//!
//!   asaled                        # local mode, 127.0.0.1:9700
//!   asaled --bind 0.0.0.0:9700    # remote B/S mode (headless server); clients
//!                                 # must present the token printed below
//!   ASALE_BIND=0.0.0.0:9700 asaled
//!
//! Endpoint config comes from the same env vars as before:
//!   ASALE_SERVER_API / ASALE_GATEWAY_API / ASALE_GATEWAY_WS / ASALE_PROXY_PORT
//!   ASALE_DATA_DIR (default ~/.asale)

fn main() -> anyhow::Result<()> {
    asale_daemon::logging::init();

    // Minimal arg parsing: --bind <addr>, --help.
    let mut bind_arg: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bind" | "-b" => bind_arg = args.next(),
            "--help" | "-h" => {
                println!("asaled — Asale client daemon\n\nUSAGE:\n  asaled [--bind <ip:port>]   (default {}, env ASALE_BIND)", asale_daemon::DEFAULT_BIND);
                return Ok(());
            }
            other => anyhow::bail!("unknown argument: {other} (see --help)"),
        }
    }
    let bind = asale_daemon::resolve_bind(bind_arg.as_deref())?;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let started = asale_daemon::start(bind).await?;
        // The token is required on every request, loopback included, so the
        // browser needs it here — there is no untokenized URL that works.
        println!("asaled ready:");
        // The fragment never reaches HTTP access logs or Referer headers.
        println!("  local:   http://127.0.0.1:{}/#token={}", started.addr.port(), started.token);
        if !started.addr.ip().is_loopback() {
            println!("  remote:  http://<this-host>:{}/#token={}", started.addr.port(), started.token);
        }
        println!("  (the token is also at {}/daemon.token, mode 0600)", asale_daemon::state::data_dir());
        tokio::signal::ctrl_c().await?;
        tracing::info!("shutting down");
        Ok::<(), anyhow::Error>(())
    })
}
