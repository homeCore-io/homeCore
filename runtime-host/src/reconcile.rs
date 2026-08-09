//! Making what is running match what core says should be running.
//!
//! Core holds the desired state and the runtime converges on it. That is the
//! whole reason a lost container volume costs nothing: the identity is the only
//! thing core cannot regenerate, and everything else here is rebuilt from the
//! placement list on the next pass.
//!
//! Convergence rather than events, deliberately. A missed message, a restart
//! mid-install, a container that was down when something was placed — all of
//! them are the same case, which is "what is here disagrees with what should
//! be", and all of them are fixed by the same code path.

use crate::adapter::{self, Adapter};
use crate::core_client::{CoreClient, Desired};
use crate::install::{self, Installed};
use crate::supervisor::{self, Supervised};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;

/// A plugin this host currently has running.
struct Running {
    version: String,
    /// The config it was launched with. A plugin reads its config once, at
    /// startup, so a change here is a restart rather than a no-op.
    config: String,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

/// What one pass did, for the log line and for the tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pass {
    pub started: Vec<String>,
    pub restarted: Vec<String>,
    pub stopped: Vec<String>,
    pub unchanged: Vec<String>,
    /// Plugins that could not be provisioned this pass, with the reason. Not
    /// fatal: one plugin whose wheelhouse is broken must not stop the others
    /// from running, and the next pass tries again.
    pub failed: Vec<(String, String)>,
}

impl Pass {
    pub fn changed_anything(&self) -> bool {
        !self.started.is_empty() || !self.restarted.is_empty() || !self.stopped.is_empty()
    }
}

/// Called with `(plugin_id, looping)` when a hosted plugin starts or stops
/// crash-looping, so the host can raise and withdraw a notice. Shared with every
/// supervisor task, hence the `Arc`.
type CrashLoopReporter = Arc<dyn Fn(&str, bool) + Send + Sync>;

pub struct Reconciler {
    root: PathBuf,
    adapter: Adapter,
    client: CoreClient,
    running: HashMap<String, Running>,
    on_crash_loop: CrashLoopReporter,
}

impl Reconciler {
    pub fn new(
        root: PathBuf,
        adapter: Adapter,
        client: CoreClient,
        on_crash_loop: impl Fn(&str, bool) + Send + Sync + 'static,
    ) -> Self {
        Self {
            root,
            adapter,
            client,
            running: HashMap::new(),
            on_crash_loop: Arc::new(on_crash_loop),
        }
    }

    pub fn hosted(&self) -> usize {
        self.running.len()
    }

    /// Ask core what should be here, and make it so.
    ///
    /// A failure to reach core leaves everything running. The alternative —
    /// stopping plugins because the list could not be fetched — would turn a
    /// core restart into an outage in every runtime at once.
    pub async fn pass(&mut self) -> Result<Pass> {
        let desired = self
            .client
            .placements()
            .await
            .context("fetching the desired state")?;
        Ok(self.converge(desired).await)
    }

    /// The decision half, with the fetch already done. Separated so it can be
    /// driven from a test without an HTTP server.
    pub async fn converge(&mut self, desired: Vec<Desired>) -> Pass {
        let mut pass = Pass::default();

        // Gone from the list means removed. Stop first, so nothing is running
        // out of a directory that is about to be deleted.
        let wanted: Vec<&str> = desired.iter().map(|d| d.plugin_id.as_str()).collect();
        let departed: Vec<String> = self
            .running
            .keys()
            .filter(|id| !wanted.contains(&id.as_str()))
            .cloned()
            .collect();
        for id in departed {
            self.stop(&id).await;
            if let Err(e) = remove_plugin_dir(&self.root, &id) {
                tracing::warn!(plugin_id = %id, error = %e, "could not remove a departed plugin");
            }
            pass.stopped.push(id);
        }

        for d in desired {
            match self.converge_one(&d).await {
                Ok(Outcome::Unchanged) => pass.unchanged.push(d.plugin_id),
                Ok(Outcome::Started) => pass.started.push(d.plugin_id),
                Ok(Outcome::Restarted) => pass.restarted.push(d.plugin_id),
                Err(e) => {
                    tracing::error!(plugin_id = %d.plugin_id, error = %format!("{e:#}"),
                        "could not provision a placed plugin — will retry");
                    pass.failed.push((d.plugin_id, format!("{e:#}")));
                }
            }
        }
        pass
    }

    async fn converge_one(&mut self, d: &Desired) -> Result<Outcome> {
        if let Some(running) = self.running.get(&d.plugin_id) {
            if running.version == d.version && running.config == d.config {
                return Ok(Outcome::Unchanged);
            }
        }
        let replacing = self.running.contains_key(&d.plugin_id);

        // Install before stopping anything. A version that fails to install
        // should leave the old one running rather than replacing a working
        // plugin with nothing.
        let installed = self.provision(d).await?;
        let argv = adapter::render(&self.adapter.launch, &installed.launch_vars())
            .with_context(|| format!("rendering the launch command for {}", d.plugin_id))?;

        if replacing {
            self.stop(&d.plugin_id).await;
        }
        self.start(d, argv);

        Ok(if replacing {
            Outcome::Restarted
        } else {
            Outcome::Started
        })
    }

    /// Get this version onto disk, reusing it when it is already there.
    ///
    /// The config is written on every pass regardless: core owns it and may
    /// change it without the version changing.
    async fn provision(&self, d: &Desired) -> Result<Installed> {
        if install::is_installed(&self.root, &d.plugin_id, &d.version) {
            let installed = install::existing(&self.root, &d.plugin_id, &d.version)?;
            install::write_config(&installed.config, &d.config)?;
            return Ok(installed);
        }

        tracing::info!(plugin_id = %d.plugin_id, version = %d.version, "fetching artifact");
        let bytes = self.client.artifact(&d.plugin_id, &d.sha256).await?;
        install::install(
            &self.root,
            &self.adapter,
            &d.plugin_id,
            &d.version,
            &bytes,
            &d.config,
        )
        .await
    }

    fn start(&mut self, d: &Desired, argv: Vec<String>) {
        let (tx, rx) = watch::channel(false);
        let plugin_id = d.plugin_id.clone();
        let notify = self.on_crash_loop.clone();
        let task = tokio::spawn(async move {
            supervisor::supervise(
                Supervised {
                    plugin_id: plugin_id.clone(),
                    argv,
                },
                rx,
                move |id, looping| notify(id, looping),
            )
            .await;
        });
        self.running.insert(
            d.plugin_id.clone(),
            Running {
                version: d.version.clone(),
                config: d.config.clone(),
                shutdown: tx,
                task,
            },
        );
    }

    /// Stop a plugin and wait for it to actually be gone.
    ///
    /// Waited on rather than fired and forgotten: the next thing that happens is
    /// usually installing over the directory it was running from.
    async fn stop(&mut self, plugin_id: &str) {
        let Some(running) = self.running.remove(plugin_id) else {
            return;
        };
        tracing::info!(%plugin_id, "stopping hosted plugin");
        let _ = running.shutdown.send(true);
        match tokio::time::timeout(STOP_TIMEOUT, running.task).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(%plugin_id, error = %e, "supervisor task failed"),
            Err(_) => tracing::warn!(
                %plugin_id,
                "supervisor did not stop within {}s — continuing anyway",
                STOP_TIMEOUT.as_secs()
            ),
        }
    }

    /// Stop everything, in the order a shutdown should.
    pub async fn stop_all(&mut self) {
        let ids: Vec<String> = self.running.keys().cloned().collect();
        for id in ids {
            self.stop(&id).await;
        }
    }
}

const STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Unchanged,
    Started,
    Restarted,
}

/// Delete everything belonging to a plugin that is no longer placed here.
///
/// Eager, per `docs/pluginRuntimesPlan.md`: the version directory is the
/// rollback mechanism for an *upgrade*, and a removal is not an upgrade. Keeping
/// environments for plugins core no longer places here would grow the volume
/// with things nothing will ever ask for again.
fn remove_plugin_dir(root: &std::path::Path, plugin_id: &str) -> Result<()> {
    let dir = install::plugin_dir(root, plugin_id)?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;

    /// `tail -f` on the config file stays up the way a real plugin does, so a
    /// restart is observable rather than being lost in supervisor backoff.
    fn adapter() -> Adapter {
        Adapter {
            kind: "test".into(),
            abi: "none".into(),
            create_env: vec!["true".into()],
            install: vec!["true".into()],
            launch: vec!["tail".into(), "-f".into(), "{config}".into()],
        }
    }

    fn artifact(id: &str, version: &str) -> Vec<u8> {
        let manifest = format!(
            "id = \"{id}\"\nname = \"T\"\nversion = \"{version}\"\n\
             runtime = \"test\"\nabi = \"none\"\narch = \"x86_64\"\n\
             entrypoint = \"t.main\"\n"
        );
        let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 0).unwrap();
        {
            let mut b = tar::Builder::new(&mut enc);
            let body = manifest.as_bytes();
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, "plugin.toml", body).unwrap();
            b.finish().unwrap();
        }
        let mut out = enc.finish().unwrap();
        out.flush().unwrap();
        out
    }

    /// Put a version on disk the way a previous pass would have, so `converge`
    /// takes the already-installed path and needs no HTTP.
    async fn preinstall(root: &Path, id: &str, version: &str) {
        install::install(
            root,
            &adapter(),
            id,
            version,
            &artifact(id, version),
            "seed = 1",
        )
        .await
        .expect("preinstall");
    }

    fn desired(id: &str, version: &str, config: &str) -> Desired {
        Desired {
            plugin_id: id.into(),
            version: version.into(),
            sha256: "unused — already installed".into(),
            config: config.into(),
        }
    }

    /// Unreachable on purpose: a test that reaches provisioning over HTTP has
    /// found a bug, because everything it needs is already on disk.
    fn reconciler(root: &Path) -> Reconciler {
        Reconciler::new(
            root.to_path_buf(),
            adapter(),
            CoreClient::new(
                reqwest::Client::new(),
                "http://127.0.0.1:1",
                "rt-test",
                "hc_sk_unused",
            ),
            |_, _| {},
        )
    }

    #[tokio::test]
    async fn a_placed_plugin_is_started_and_then_left_alone() {
        let root = tempfile::tempdir().unwrap();
        preinstall(root.path(), "plugin.t", "0.1.0").await;
        let mut r = reconciler(root.path());

        let first = r
            .converge(vec![desired("plugin.t", "0.1.0", "a = 1")])
            .await;
        assert_eq!(first.started, vec!["plugin.t"]);
        assert_eq!(r.hosted(), 1);

        // The second pass is the common case — nothing must move.
        let second = r
            .converge(vec![desired("plugin.t", "0.1.0", "a = 1")])
            .await;
        assert_eq!(second.unchanged, vec!["plugin.t"]);
        assert!(!second.changed_anything(), "{second:?}");

        r.stop_all().await;
    }

    /// A plugin reads its config once, at startup. Writing a new one without
    /// restarting would leave core's UI showing a setting the plugin is not
    /// using — which is worse than not applying it at all, because it looks
    /// applied.
    #[tokio::test]
    async fn a_config_change_restarts_the_plugin() {
        let root = tempfile::tempdir().unwrap();
        preinstall(root.path(), "plugin.t", "0.1.0").await;
        let mut r = reconciler(root.path());

        r.converge(vec![desired("plugin.t", "0.1.0", "a = 1")])
            .await;
        let pass = r
            .converge(vec![desired("plugin.t", "0.1.0", "a = 2")])
            .await;

        assert_eq!(pass.restarted, vec!["plugin.t"]);
        let cfg = install::version_dir(root.path(), "plugin.t", "0.1.0")
            .unwrap()
            .join("config.toml");
        assert_eq!(std::fs::read_to_string(cfg).unwrap(), "a = 2");

        r.stop_all().await;
    }

    #[tokio::test]
    async fn an_upgrade_switches_versions_and_keeps_the_old_one_for_rollback() {
        let root = tempfile::tempdir().unwrap();
        preinstall(root.path(), "plugin.t", "0.1.0").await;
        preinstall(root.path(), "plugin.t", "0.2.0").await;
        let mut r = reconciler(root.path());

        r.converge(vec![desired("plugin.t", "0.1.0", "a = 1")])
            .await;
        let pass = r
            .converge(vec![desired("plugin.t", "0.2.0", "a = 1")])
            .await;
        assert_eq!(pass.restarted, vec!["plugin.t"]);

        // Rollback is a placement change, not a re-download.
        assert!(
            install::is_installed(root.path(), "plugin.t", "0.1.0"),
            "the previous version must survive an upgrade"
        );

        r.stop_all().await;
    }

    /// Gone from the list is core saying it was removed. Stopping without
    /// deleting would leave a volume full of environments nothing will ask for
    /// again.
    #[tokio::test]
    async fn a_departed_plugin_is_stopped_and_deleted() {
        let root = tempfile::tempdir().unwrap();
        preinstall(root.path(), "plugin.t", "0.1.0").await;
        let mut r = reconciler(root.path());

        r.converge(vec![desired("plugin.t", "0.1.0", "a = 1")])
            .await;
        let pass = r.converge(Vec::new()).await;

        assert_eq!(pass.stopped, vec!["plugin.t"]);
        assert_eq!(r.hosted(), 0);
        assert!(
            !install::plugin_dir(root.path(), "plugin.t")
                .unwrap()
                .exists(),
            "a removed plugin leaves nothing behind"
        );
    }

    /// One plugin that cannot be provisioned must not take the others down with
    /// it. `plugin.broken` is not installed and core is unreachable, so it fails
    /// where a real one would fail on a bad wheelhouse.
    #[tokio::test]
    async fn a_plugin_that_cannot_be_provisioned_does_not_block_the_rest() {
        let root = tempfile::tempdir().unwrap();
        preinstall(root.path(), "plugin.good", "0.1.0").await;
        let mut r = reconciler(root.path());

        let pass = r
            .converge(vec![
                desired("plugin.broken", "9.9.9", "a = 1"),
                desired("plugin.good", "0.1.0", "a = 1"),
            ])
            .await;

        assert_eq!(pass.started, vec!["plugin.good"]);
        assert_eq!(pass.failed.len(), 1, "{pass:?}");
        assert_eq!(pass.failed[0].0, "plugin.broken");
        assert_eq!(r.hosted(), 1);

        r.stop_all().await;
    }

    /// A version that will not install must leave the running one alone. The
    /// failure mode being avoided is an upgrade that stops a working plugin and
    /// then cannot start its replacement.
    #[tokio::test]
    async fn a_failed_upgrade_leaves_the_running_version_up() {
        let root = tempfile::tempdir().unwrap();
        preinstall(root.path(), "plugin.t", "0.1.0").await;
        let mut r = reconciler(root.path());

        r.converge(vec![desired("plugin.t", "0.1.0", "a = 1")])
            .await;
        // 0.2.0 was never installed and core is unreachable.
        let pass = r
            .converge(vec![desired("plugin.t", "0.2.0", "a = 1")])
            .await;

        assert_eq!(pass.failed.len(), 1, "{pass:?}");
        assert_eq!(r.hosted(), 1, "the old version must still be running");

        r.stop_all().await;
    }
}
