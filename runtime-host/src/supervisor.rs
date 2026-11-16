//! Supervising the plugins inside this runtime.
//!
//! This is `plugin_launcher` for a different substrate. The semantics are copied
//! deliberately — same minimum and maximum backoff, same "a long enough run
//! resets it" rule — because an operator should not have to learn two supervision
//! behaviours depending on which side of a container a plugin happens to run on.
//!
//! A process per plugin, which falls out of the environment-per-plugin decision
//! (two plugins wanting different dependency versions cannot share an
//! interpreter) and also contains a crash.

use std::time::Duration;
use tokio::process::Command;
use tokio::sync::watch;

/// Matches `homecore/src/plugin_launcher.rs`.
const MIN_BACKOFF: u64 = 2;
const MAX_BACKOFF: u64 = 60;
/// A run lasting at least this long is treated as healthy and resets the backoff.
const HEALTHY_UPTIME: Duration = Duration::from_secs(60);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Next backoff after a process exits.
///
/// Separated from the loop so the escalation rule can be tested without spawning
/// anything — the behaviour that matters here is arithmetic, and it is the part
/// that would otherwise only be observable by waiting a minute.
pub fn next_backoff(current: u64, uptime: Duration) -> u64 {
    if uptime >= HEALTHY_UPTIME {
        MIN_BACKOFF
    } else {
        (current * 2).min(MAX_BACKOFF)
    }
}

/// How many consecutive short runs before this is worth telling the operator
/// about. Three restarts inside a couple of minutes is a crash loop, not a blip.
pub const CRASH_LOOP_THRESHOLD: u32 = 3;

pub struct Supervised {
    /// The plugin id, as homeCore knows it.
    pub plugin_id: String,
    /// Fully rendered argv — program first, config path last.
    pub argv: Vec<String>,
    /// Extra environment: where its UI folder is (`HC_PLUGIN_UI_DIR`).
    pub env: Vec<(String, String)>,
}

/// Run one plugin until shutdown, restarting it with backoff.
///
/// `on_crash_loop` is called once the plugin has failed to stay up
/// [`CRASH_LOOP_THRESHOLD`] times in a row, so the caller can raise a notice.
/// It is called again with `false` when the plugin recovers, because a notice is
/// state and one that is never withdrawn is worse than one never raised.
pub async fn supervise<F>(
    entry: Supervised,
    mut shutdown: watch::Receiver<bool>,
    mut on_crash_loop: F,
) where
    F: FnMut(&str, bool) + Send,
{
    let mut backoff = MIN_BACKOFF;
    let mut consecutive_failures: u32 = 0;
    let mut reported = false;

    loop {
        if *shutdown.borrow() {
            tracing::info!(plugin_id = %entry.plugin_id, "supervisor stopping for shutdown");
            break;
        }

        let (program, args) = entry.argv.split_first().expect("argv is never empty");
        tracing::info!(plugin_id = %entry.plugin_id, %program, "launching hosted plugin");

        let started = std::time::Instant::now();
        let mut child = match Command::new(program)
            .args(args)
            .envs(entry.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(mut c) => {
                // Its output goes to the container log, prefixed with its id so
                // two plugins can be told apart, and to the runtime's page.
                if let Some(out) = c.stdout.take() {
                    tokio::spawn(relay(entry.plugin_id.clone(), out, false));
                }
                if let Some(err) = c.stderr.take() {
                    tokio::spawn(relay(entry.plugin_id.clone(), err, true));
                }
                c
            }
            Err(e) => {
                tracing::error!(
                    plugin_id = %entry.plugin_id, %program, error = %e,
                    "could not start hosted plugin — retrying in {backoff}s"
                );
                consecutive_failures += 1;
                if consecutive_failures >= CRASH_LOOP_THRESHOLD && !reported {
                    on_crash_loop(&entry.plugin_id, true);
                    reported = true;
                }
                if sleep_or_shutdown(backoff, &mut shutdown).await {
                    break;
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };

        let uptime = tokio::select! {
            result = child.wait() => {
                let uptime = started.elapsed();
                match result {
                    Ok(status) if status.success() => tracing::info!(
                        plugin_id = %entry.plugin_id, uptime_secs = uptime.as_secs(),
                        "hosted plugin exited cleanly"
                    ),
                    Ok(status) => tracing::warn!(
                        plugin_id = %entry.plugin_id, code = ?status.code(),
                        uptime_secs = uptime.as_secs(), "hosted plugin exited with an error"
                    ),
                    Err(e) => tracing::error!(
                        plugin_id = %entry.plugin_id, error = %e, "wait() failed"
                    ),
                }
                uptime
            }
            _ = wait_for_shutdown(&mut shutdown) => {
                stop_child(&entry.plugin_id, &mut child).await;
                break;
            }
        };

        if uptime >= HEALTHY_UPTIME {
            consecutive_failures = 0;
            if reported {
                // It came back. Withdraw the notice rather than leaving the
                // operator looking at a problem that resolved itself.
                on_crash_loop(&entry.plugin_id, false);
                reported = false;
            }
        } else {
            consecutive_failures += 1;
            if consecutive_failures >= CRASH_LOOP_THRESHOLD && !reported {
                on_crash_loop(&entry.plugin_id, true);
                reported = true;
            }
        }

        backoff = next_backoff(backoff, uptime);
        tracing::warn!(
            plugin_id = %entry.plugin_id, backoff_secs = backoff,
            "hosted plugin will restart after backoff"
        );
        if sleep_or_shutdown(backoff, &mut shutdown).await {
            break;
        }
    }
}

/// Copy one of a plugin's output streams, line by line, to the container log
/// and the runtime's log book. Ends when the plugin closes the stream.
async fn relay(plugin_id: String, stream: impl tokio::io::AsyncRead + Unpin, stderr: bool) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if stderr {
            eprintln!("{plugin_id} | {line}");
        } else {
            println!("{plugin_id} | {line}");
        }
        crate::logs::book().push(&plugin_id, &line);
    }
}

/// Sleep, unless shutdown arrives first. Returns true when it did.
async fn sleep_or_shutdown(secs: u64, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(secs)) => false,
        _ = wait_for_shutdown(shutdown) => true,
    }
}

async fn wait_for_shutdown(rx: &mut watch::Receiver<bool>) {
    while rx.changed().await.is_ok() {
        if *rx.borrow() {
            return;
        }
    }
    // The sender is gone, which means the host is going away too.
    std::future::pending::<()>().await;
}

/// Ask first (SIGTERM), so a plugin can leave what it controls safe, and kill it
/// only if it has not gone within [`SHUTDOWN_GRACE`]. The same as core's
/// `kill_child`. This used to only wait, so every stop was a SIGKILL after a
/// pause: a plugin was never told to stop, and never ran its handler.
async fn stop_child(plugin_id: &str, child: &mut tokio::process::Child) {
    tracing::info!(%plugin_id, "shutdown requested — asking hosted plugin to exit");
    if ask_to_stop(child)
        && tokio::time::timeout(SHUTDOWN_GRACE, child.wait())
            .await
            .is_ok()
    {
        return;
    }
    tracing::warn!(%plugin_id, "hosted plugin did not exit when asked — killing");
    let _ = child.start_kill();
    let _ = tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await;
}

/// SIGTERM to a running child. False when there is no way to ask (not unix,
/// or it has already gone).
#[cfg(unix)]
fn ask_to_stop(child: &tokio::process::Child) -> bool {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    match child.id() {
        Some(pid) => kill(Pid::from_raw(pid as i32), Signal::SIGTERM).is_ok(),
        None => false,
    }
}

#[cfg(not(unix))]
fn ask_to_stop(_child: &tokio::process::Child) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Escalation matches core's: double, capped at a minute.
    #[test]
    fn a_crash_doubles_the_backoff_up_to_the_cap() {
        let brief = Duration::from_secs(1);
        assert_eq!(next_backoff(2, brief), 4);
        assert_eq!(next_backoff(4, brief), 8);
        assert_eq!(next_backoff(32, brief), 60);
        assert_eq!(next_backoff(60, brief), 60, "capped");
    }

    /// ...and a run that stayed up resets it, so one bad afternoon does not
    /// leave a healthy plugin on a minute-long restart delay forever.
    #[test]
    fn a_healthy_run_resets_the_backoff() {
        assert_eq!(next_backoff(60, HEALTHY_UPTIME), MIN_BACKOFF);
        assert_eq!(
            next_backoff(60, HEALTHY_UPTIME + Duration::from_secs(1)),
            MIN_BACKOFF
        );
    }

    /// The boundary is inclusive on core's side; matching it exactly is the
    /// whole point of copying the constants rather than picking new ones.
    #[test]
    fn the_healthy_boundary_is_inclusive() {
        assert_eq!(next_backoff(8, HEALTHY_UPTIME), MIN_BACKOFF);
        assert_eq!(
            next_backoff(8, HEALTHY_UPTIME - Duration::from_millis(1)),
            16
        );
    }

    /// A plugin that exits immediately, repeatedly, must reach the operator —
    /// and must stop being reported once it recovers.
    #[tokio::test]
    async fn a_crash_loop_is_reported_once_and_withdrawn_on_recovery() {
        use std::sync::{Arc, Mutex};

        let events: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let (tx, rx) = watch::channel(false);

        let handle = tokio::spawn(async move {
            supervise(
                Supervised {
                    plugin_id: "plugin.flaky".into(),
                    // Exits immediately, every time.
                    argv: vec!["false".to_string(), "/tmp/ignored.toml".to_string()],
                    env: Vec::new(),
                },
                rx,
                move |id, looping| sink.lock().unwrap().push((id.to_string(), looping)),
            )
            .await;
        });

        // Three failures at 2 + 4 + 8s of backoff would be slow to wait out, so
        // assert on the first report and then shut down.
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if events.lock().unwrap().iter().any(|(_, l)| *l) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("a repeatedly-failing plugin should be reported as looping");

        let _ = tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;

        let seen = events.lock().unwrap().clone();
        let reports: Vec<_> = seen.iter().filter(|(_, l)| *l).collect();
        assert_eq!(
            reports.len(),
            1,
            "reported once, not once per restart: {seen:?}"
        );
        assert_eq!(reports[0].0, "plugin.flaky");
    }

    /// Shutdown must stop the loop rather than restarting into a closing host.
    #[tokio::test]
    async fn shutdown_ends_supervision() {
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            supervise(
                Supervised {
                    plugin_id: "plugin.sleeper".into(),
                    argv: vec!["sleep".to_string(), "30".to_string()],
                    env: Vec::new(),
                },
                rx,
                |_, _| {},
            )
            .await;
        });

        tokio::time::sleep(Duration::from_millis(300)).await;
        tx.send(true).unwrap();

        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("supervisor should stop promptly on shutdown")
            .unwrap();
    }

    /// A plugin is asked to stop, not just killed after a wait. `sh` running a
    /// trap exits with 42 on SIGTERM; a SIGKILL would leave no exit code at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stopping_plugin_is_asked_first() {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "trap 'exit 42' TERM; while :; do sleep 0.1; done"])
            .spawn()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started = std::time::Instant::now();
        stop_child("plugin.test", &mut child).await;
        let status = child.wait().await.unwrap();
        assert_eq!(status.code(), Some(42), "{status:?}");
        assert!(
            started.elapsed() < SHUTDOWN_GRACE,
            "it waited out the grace period"
        );
    }
}
