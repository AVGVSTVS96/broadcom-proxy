use std::{
    os::fd::{FromRawFd, RawFd},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use broadcom_proxy::{
    app::{App, Endpoints},
    config::Config,
    manage, oauth, sampler,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path =
        std::env::args_os().nth(1).map_or_else(|| PathBuf::from("~/.config/broadcom-proxy/config.json"), PathBuf::from);
    let path = broadcom_proxy::config::expand_home(&path);
    let (config, revision) = Config::load(&path)?;
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

    axum::serve(listener, broadcom_proxy::router(app)).with_graceful_shutdown(shutdown()).await?;
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

/// SIGTERM or Ctrl-C: stop taking connections and let requests in flight finish.
async fn shutdown() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}
