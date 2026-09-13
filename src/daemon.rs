//! The `byssusd` daemon: startup, event loop, reload and shutdown.
//!
//! See `docs/DESIGN.md`, "Daemon lifecycle".

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use rustix::event::{Timespec, epoll};

use crate::app::{self, ConfigSource};
use crate::config::{Config, OwnershipPolicy};
use crate::notify::Notifier;
use crate::privileges::plan::Goal;
use crate::reconcile::{self, Trigger};
use crate::runtime::Runtime;
use crate::signals::SignalFd;
use crate::state::{State, StateStore};
use crate::watcher::Watcher;

/// Daemon command-line options.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Configuration location.
    pub config: ConfigSource,
    /// Service user override.
    pub user: Option<String>,
    /// Permit running as root without a service user.
    pub allow_root: bool,
    /// Permit target roots on slave mounts.
    pub allow_slave_namespace: bool,
}

const TOKEN_INOTIFY: u64 = 1;
const TOKEN_SIGNAL: u64 = 2;

/// Membership changes are applied after this quiet period, so that a burst of
/// events (for example a directory being deleted file by file) is seen as a
/// whole — including any removal of the membership directory itself.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// Upper bound on how long continuous changes can postpone a pass.
const MAX_DEBOUNCE: Duration = Duration::from_secs(1);

struct Daemon {
    options: Options,
    config: Config,
    runtime: Runtime,
    watcher: Watcher,
    store: StateStore,
    state: State,
    degraded: reconcile::Degraded,
    unique_supported: bool,
    epoll: std::os::fd::OwnedFd,
    next_resync: Option<Instant>,
    /// A pending inotify-triggered pass: (first change, due time).
    pending: Option<(Instant, Instant)>,
    /// Observation notes already logged.
    notes: reconcile::NoteLog,
    /// systemd notifications.
    notifier: Notifier,
    /// Why the most recent reload was rejected, until one succeeds.
    last_reload_error: Option<String>,
}

/// Runs the daemon until `SIGTERM` or `SIGINT`.
pub fn run(options: Options) -> anyhow::Result<()> {
    let notifier = Notifier::from_env().unwrap_or_else(|e| {
        tracing::warn!(msg = "ignoring invalid NOTIFY_SOCKET", error = %e);
        Notifier::disabled()
    });
    let loaded = app::load_config(&options.config, OwnershipPolicy::Enforce)?;
    let config = loaded.config;
    app::set_umask();
    let user = options.user.clone().or_else(|| config.daemon.user.clone());
    app::normalize_privileges(Goal::KeepSysAdmin, user.as_deref(), options.allow_root)?;
    let environment = app::check_environment()?;
    app::verify_config_paths(&config)?;
    let writer = app::open_state_for_write(&config)?;
    let mut runtime = Runtime::open(&config)?;
    app::check_propagation(&mut runtime, options.allow_slave_namespace)?;

    // Watches and signals are set up before the initial reconcile so that no
    // change during startup is missed.
    let signals = SignalFd::new(&[libc::SIGHUP, libc::SIGTERM, libc::SIGINT])
        .context("cannot set up signal handling")?;
    let watcher = Watcher::new(&runtime)?;
    let epoll = epoll::create(epoll::CreateFlags::CLOEXEC).context("epoll_create")?;
    epoll::add(
        &epoll,
        signals.as_fd(),
        epoll::EventData::new_u64(TOKEN_SIGNAL),
        epoll::EventFlags::IN,
    )?;
    epoll::add(
        &epoll,
        watcher.as_fd(),
        epoll::EventData::new_u64(TOKEN_INOTIFY),
        epoll::EventFlags::IN,
    )?;

    let crate::app::Writer { store, lock, state } = writer;
    let mut daemon = Daemon {
        options,
        config,
        runtime,
        watcher,
        store,
        state,
        degraded: reconcile::Degraded::default(),
        unique_supported: environment.features.unique_mount_ids(),
        epoll,
        next_resync: None,
        pending: None,
        notes: reconcile::NoteLog::default(),
        notifier,
        last_reload_error: None,
    };
    tracing::info!(
        msg = "byssusd started",
        version = crate::VERSION,
        groups = daemon.config.groups.len(),
        records = daemon.state.len(),
    );
    daemon.pass(Trigger::Startup);
    // Ready only once the startup reconcile has populated the views, so units
    // ordered after byssusd (such as container runtimes) see every member.
    daemon.notifier.ready(&daemon.status_text());
    let result = daemon.event_loop(&signals);
    daemon.notifier.stopping();

    if let Err(e) = daemon.store.save(&daemon.state) {
        tracing::error!(msg = "cannot write state file on shutdown", error = %e);
    }
    drop(lock);
    drop(environment);
    tracing::info!(msg = "byssusd stopped; mounts are preserved");
    result
}

impl Daemon {
    fn pass(&mut self, trigger: Trigger) {
        let watcher = &mut self.watcher;
        let pass = reconcile::run_pass(
            &mut self.runtime,
            &mut self.state,
            &self.store,
            &self.degraded,
            self.unique_supported,
            trigger,
            &mut self.notes,
            &mut |runtime| watcher.sync(runtime),
        );
        for note in &pass.observed.notes {
            match note {
                reconcile::observe::Note::MembershipDeleted { group } => {
                    self.degraded.groups.insert(group.clone());
                }
                reconcile::observe::Note::SetRootDeleted { set } => {
                    self.degraded.sets.insert(set.clone());
                }
                _ => {}
            }
        }
        let level_debug = pass.plan.steps.is_empty() && pass.plan.findings.is_empty();
        if level_debug {
            tracing::debug!(msg = "reconcile complete", trigger = %trigger, steps = 0);
        } else {
            tracing::info!(
                msg = "reconcile complete",
                trigger = %trigger,
                steps = pass.plan.steps.len(),
                completed = pass.execution.completed(),
                failed = pass.execution.failures(),
                findings = pass.plan.findings.len(),
            );
        }
        self.next_resync = self
            .config
            .daemon
            .resync_interval
            .map(|interval| Instant::now() + interval);
        self.notifier.status(&self.status_text());
    }

    fn status_text(&self) -> String {
        use std::fmt::Write as _;
        let mut text = format!(
            "{} group(s), {} mount(s)",
            self.runtime.groups.len(),
            self.state.len()
        );
        if !self.degraded.is_empty() {
            let names: Vec<String> = self
                .degraded
                .groups
                .iter()
                .map(ToString::to_string)
                .chain(self.degraded.sets.iter().map(reconcile::set_label))
                .collect();
            let _ = write!(text, "; degraded: {}", names.join(", "));
        }
        if let Some(error) = &self.last_reload_error {
            let _ = write!(
                text,
                "; last reload failed, running previous configuration: {error}"
            );
        }
        text
    }

    fn event_loop(&mut self, signals: &SignalFd) -> anyhow::Result<()> {
        let mut events = Vec::with_capacity(4);
        loop {
            let deadline = [self.next_resync, self.pending.map(|(_, due)| due)]
                .into_iter()
                .flatten()
                .min();
            let timeout = deadline.map(|deadline| {
                duration_to_timespec(deadline.saturating_duration_since(Instant::now()))
            });
            events.clear();
            match epoll::wait(
                &self.epoll,
                rustix::buffer::spare_capacity(&mut events),
                timeout.as_ref(),
            ) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e).context("epoll_wait"),
            }

            let mut run_pass = None;
            for event in &events {
                match event.data.u64() {
                    TOKEN_SIGNAL => {
                        while let Some(signal) = signals.read().context("reading signalfd")? {
                            match i32::try_from(signal).unwrap_or(-1) {
                                libc::SIGHUP => {
                                    self.notifier.reloading();
                                    if self.reload() {
                                        self.pending = None;
                                        self.pass(Trigger::Reload);
                                    }
                                    self.notifier.ready(&self.status_text());
                                }
                                libc::SIGTERM | libc::SIGINT => {
                                    tracing::info!(msg = "shutdown requested", signal = signal);
                                    return Ok(());
                                }
                                other => tracing::debug!(msg = "ignoring signal", signal = other),
                            }
                        }
                    }
                    TOKEN_INOTIFY => self.handle_inotify()?,
                    _ => {}
                }
            }

            let now = Instant::now();
            if run_pass.is_none() && self.pending.is_some_and(|(_, due)| now >= due) {
                run_pass = Some(Trigger::Inotify);
            }
            if run_pass.is_none() && self.next_resync.is_some_and(|due| now >= due) {
                run_pass = Some(Trigger::Resync);
            }
            if let Some(trigger) = run_pass {
                self.pending = None;
                self.pass(trigger);
            }
        }
    }

    fn handle_inotify(&mut self) -> anyhow::Result<()> {
        let changes = self.watcher.drain().context("reading inotify events")?;
        for group in &changes.lost {
            if self.degraded.groups.insert(group.clone()) {
                tracing::error!(
                    op = "degrade",
                    group = %group,
                    msg = "membership directory was removed, moved or unmounted; keeping existing mounts and making no changes to this group until configuration is reloaded (SIGHUP)",
                );
            }
        }
        for set in &changes.lost_sets {
            if self.degraded.sets.insert(set.clone()) {
                tracing::error!(
                    op = "degrade",
                    group = %reconcile::set_label(set),
                    msg = "group set membership root was removed, moved or unmounted; keeping existing mounts and making no changes to this set until configuration is reloaded (SIGHUP)",
                );
            }
        }
        if changes.overflow {
            tracing::warn!(msg = "inotify queue overflowed; running a full reconcile");
        }
        if changes.membership_changed || changes.overflow {
            let now = Instant::now();
            let first = self.pending.map_or(now, |(first, _)| first);
            self.pending = Some((first, (now + DEBOUNCE).min(first + MAX_DEBOUNCE)));
        }
        Ok(())
    }

    /// Transactional reload. Returns whether the new configuration was
    /// applied.
    fn reload(&mut self) -> bool {
        tracing::info!(msg = "reloading configuration");
        match self.prepare_reload() {
            Ok((config, runtime, watcher)) => {
                if let Err(e) = self.swap_watcher(&watcher) {
                    tracing::error!(msg = "reload failed; keeping previous configuration", error = %format!("{e:#}"));
                    self.last_reload_error = Some(first_line(&format!("{e:#}")));
                    return false;
                }
                self.config = config;
                self.runtime = runtime;
                self.watcher = watcher;
                self.degraded = reconcile::Degraded::default();
                self.last_reload_error = None;
                tracing::info!(
                    msg = "configuration reloaded",
                    groups = self.config.groups.len()
                );
                true
            }
            Err(e) => {
                tracing::error!(msg = "reload failed; keeping previous configuration", error = %format!("{e:#}"));
                self.last_reload_error = Some(first_line(&format!("{e:#}")));
                false
            }
        }
    }

    fn prepare_reload(&self) -> anyhow::Result<(Config, Runtime, Watcher)> {
        let loaded = app::load_config(&self.options.config, OwnershipPolicy::Enforce)?;
        let config = loaded.config;
        if config.daemon.state_dir != self.config.daemon.state_dir {
            bail!(
                "daemon.state_dir changed from {} to {}; restart byssusd to change it",
                self.config.daemon.state_dir,
                config.daemon.state_dir
            );
        }
        if config.daemon.user != self.config.daemon.user {
            tracing::warn!(msg = "daemon.user changed; it takes effect on restart");
        }
        app::verify_config_paths(&config)?;
        let mut runtime = Runtime::open(&config)?;
        app::check_propagation(&mut runtime, self.options.allow_slave_namespace)?;
        let watcher = Watcher::new(&runtime)?;
        Ok((config, runtime, watcher))
    }

    fn swap_watcher(&self, new: &Watcher) -> anyhow::Result<()> {
        epoll::add(
            &self.epoll,
            new.as_fd(),
            epoll::EventData::new_u64(TOKEN_INOTIFY),
            epoll::EventFlags::IN,
        )
        .context("registering new inotify instance")?;
        epoll::delete(&self.epoll, self.watcher.as_fd())
            .context("removing old inotify instance")?;
        Ok(())
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().to_owned()
}

fn duration_to_timespec(d: Duration) -> Timespec {
    Timespec {
        tv_sec: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(d.subsec_nanos()),
    }
}
