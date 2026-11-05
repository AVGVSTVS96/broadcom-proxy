use std::{
    os::fd::{FromRawFd, RawFd},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use broadcom_proxy::{
    app::{App, Endpoints},
    config::{Config, ensure_secret},
    manage,
    oauth::{self, Provider, Status},
    sampler,
};

/// How long requests in flight get to finish after SIGTERM.
const DRAIN: Duration = Duration::from_mins(10);

const USAGE: &str = "\
usage: broadcom-proxy [config.json]        run the pool
       broadcom-proxy login [config.json]  add a Claude account

The config defaults to ~/.config/broadcom-proxy/config.json; without one, every default applies.";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let login = args.first().is_some_and(|a| a == "login");
    if login {
        args.remove(0);
    }
    let path = args.first().map_or_else(|| PathBuf::from("~/.config/broadcom-proxy/config.json"), PathBuf::from);
    let path = broadcom_proxy::config::expand_home(&path);
    let (config, revision) = Config::load(&path)?;
    for (file, what) in [(&config.client_keys_file, "client key"), (&config.management_key_file, "management key")] {
        if ensure_secret(file)? {
            println!("wrote a new {what} to {}", file.display());
        }
    }
    if login {
        return add_account(Arc::new(App::new(path, config, revision, Endpoints::default())?)).await;
    }
    let listener = listener(&config)?;
    let app = Arc::new(App::new(path, config, revision, Endpoints::default())?);
    {
        let state = app.state();
        println!(
            "broadcom-proxy on {}: {} plan accounts, {} API keys, account owner: {}",
            listener.local_addr()?,
            state.accounts.len(),
            state.keys.len(),
            app.config.account_owner
        );
    }

    tokio::spawn(every(app.clone(), Duration::from_mins(1), |app| async move { sampler::sample(&app).await }));
    if app.config.account_owner {
        tokio::spawn(every(app.clone(), Duration::from_mins(15), |app| async move { oauth::refresh_due(&app).await }));
    }
    tokio::spawn(manage::run_live(app.clone()));

    let stopping = Arc::new(tokio::sync::Notify::new());
    let server = axum::serve(listener, broadcom_proxy::router(app.clone())).with_graceful_shutdown({
        let stopping = stopping.clone();
        async move {
            shutdown().await;
            app.live.close();
            stopping.notify_one();
        }
    });
    tokio::select! {
        served = server => served?,
        () = async { stopping.notified().await; tokio::time::sleep(DRAIN).await } => {
            eprintln!("stopping with requests still running after {} minutes", DRAIN.as_secs() / 60);
        }
    }
    Ok(())
}

/// Signs a Claude account in from the terminal and saves its login with the others.
async fn add_account(app: Arc<App>) -> anyhow::Result<()> {
    let started = oauth::start(&app, Provider::Claude, None).await?;
    println!("Open this link, sign in to the Claude account to add, and paste the code it shows:\n\n{}\n", started.url);
    let mut code = String::new();
    std::io::stdin().read_line(&mut code)?;
    let code = code.trim().split('#').next().unwrap_or_default();
    if let Status::Error { error } = oauth::finish_claude(&app, &started.state, code).await {
        anyhow::bail!(error);
    }
    println!("Saved in {}. A running proxy picks it up within a minute.", app.config.credentials_dir.display());
    Ok(())
}

/// The socket systemd passed in, which holds the port across restarts, or a fresh one.
fn listener(config: &Config) -> anyhow::Result<tokio::net::TcpListener> {
    const FIRST_PASSED_FD: RawFd = 3;
    let passed = std::env::var("LISTEN_PID").ok().and_then(|p| p.parse::<u32>().ok()) == Some(std::process::id());
    let socket = if passed {
        // SAFETY: systemd hands this process the listening socket as fd 3 and nothing else owns it.
        unsafe { std::net::TcpListener::from_raw_fd(FIRST_PASSED_FD) }
    } else {
        std::net::TcpListener::bind(config.listen).with_context(|| format!("binding {}", config.listen))?
    };
    socket.set_nonblocking(true)?;
    Ok(tokio::net::TcpListener::from_std(socket)?)
}

/// Runs `job` now and then every `period`.
async fn every<F, Fut>(app: Arc<App>, period: Duration, job: F)
where
    F: Fn(Arc<App>) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        job(app.clone()).await;
    }
}

/// SIGTERM or Ctrl-C.
async fn shutdown() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}
