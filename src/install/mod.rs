//! `herdr-triggersd install` and `uninstall`: the mod, its key bindings, the
//! config link and the login service for the Herdr desktop app.
//!
//! The whole plan is computed before anything is written, including the
//! checks that each write can succeed, so a path that cannot be written
//! stops the run with nothing changed. A bad `keys.json` (invalid JSON, the
//! wrong shape, unwritable) only turns the two key lines into skips; the
//! other steps go ahead. A step that fails while being applied stops there;
//! earlier steps stay, and re-running finishes the job because every step
//! is idempotent.

pub mod keys;
pub mod service;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use service::{Cmd, Manager, System};

const MOD_NAME: &str = "herdr-triggers";
const MOD_FILES: [(&str, &str); 2] = [
    ("main.js", include_str!("../../mods/herdr-triggers/main.js")),
    (
        "mod.json",
        include_str!("../../mods/herdr-triggers/mod.json"),
    ),
];
const KEYS: &str = include_str!("../../keys.json");

/// Every path and fact the plan depends on, so tests can point it at a
/// temporary home.
#[derive(Clone, Debug)]
pub struct Context {
    pub home: PathBuf,
    /// The app's home (`HERDR_APP_HOME` or `~/.config/herdr-app`).
    pub app_home: PathBuf,
    /// Where the core daemon reads its rules (`~/.config/herdr-triggers`).
    pub config_link: PathBuf,
    /// The TUI plugin's config directory the link points at.
    pub tui_config: PathBuf,
    pub manager: Option<Manager>,
    pub bin: PathBuf,
    /// The service's environment, `PATH` first.
    pub env: Vec<(String, String)>,
    pub uid: u32,
}

impl Context {
    pub fn from_env(tui_config: PathBuf) -> Result<Self, String> {
        let home = crate::core::paths::home_dir();
        if home.as_os_str().is_empty() {
            return Err("HOME is not set".to_string());
        }
        if !home.is_absolute() {
            return Err(format!("HOME must be absolute, not {}", home.display()));
        }
        absolute_dirs(|name| std::env::var(name).ok())?;
        // With HOME absolute and a relative XDG_CONFIG_HOME ignored, only
        // HERDR_PLUGIN_CONFIG_DIR can make this relative.
        if !tui_config.is_absolute() {
            return Err(format!(
                "HERDR_PLUGIN_CONFIG_DIR must be absolute, not {}",
                tui_config.display()
            ));
        }
        let path_env = std::env::var("PATH")
            .ok()
            .filter(|p| !p.is_empty())
            .ok_or("PATH is not set; the service needs it to run rule scripts")?;
        let exe = std::env::current_exe().map_err(|e| format!("cannot locate own binary: {e}"))?;
        Ok(Self {
            app_home: crate::core::paths::app_home(),
            config_link: crate::core::paths::config_dir(),
            tui_config,
            manager: service::platform_manager(),
            bin: service::choose_bin(&path_env, &exe),
            env: service::service_env(
                &path_env,
                &crate::core::paths::config_home(),
                &crate::core::paths::state_home(),
                |name| std::env::var(name).ok(),
            ),
            uid: service::uid(),
            home,
        })
    }

    fn mod_dir(&self) -> PathBuf {
        self.app_home.join("mods").join(MOD_NAME)
    }

    fn keys_path(&self) -> PathBuf {
        self.app_home.join("keys.json")
    }

    /// `path` for the plan, with the home directory as `~`.
    fn show(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Install,
    Uninstall,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Create,
    Update,
    Unchanged,
    Remove,
    Stop,
    /// The file is in place but the manager does not have it enabled or
    /// loaded: a failed enable or bootstrap, or a manual disable.
    Enable,
    /// Enabled or loaded, but not running.
    Start,
    Skip(String),
}

impl Status {
    /// The status column; a skip's reason goes in the detail column, so one
    /// long reason does not widen every row.
    fn word(&self) -> &'static str {
        match self {
            Status::Create => "create",
            Status::Update => "update",
            Status::Unchanged => "unchanged",
            Status::Remove => "remove",
            Status::Stop => "stop",
            Status::Enable => "enable",
            Status::Start => "start",
            Status::Skip(_) => "skip",
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Skip(why) => write!(f, "skip: {why}"),
            other => f.write_str(other.word()),
        }
    }
}

#[derive(Debug)]
pub struct Line {
    pub status: Status,
    pub item: String,
    pub detail: String,
}

impl Line {
    fn shown_detail(&self) -> String {
        match &self.status {
            Status::Skip(why) if self.detail.is_empty() => why.clone(),
            Status::Skip(why) => format!("{}: {why}", self.detail),
            _ => self.detail.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Write(PathBuf, String),
    Remove(PathBuf),
    RemoveDirIfEmpty(PathBuf),
    Symlink {
        link: PathBuf,
        target: PathBuf,
    },
    Run(Cmd),
    /// Bootout only when launchd has the job loaded.
    BootoutIfLoaded(u32),
    StopDaemon(i32),
}

#[derive(Debug)]
struct Step {
    name: String,
    actions: Vec<Action>,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub lines: Vec<Line>,
    steps: Vec<Step>,
}

impl Plan {
    fn line(&mut self, status: Status, item: impl Into<String>, detail: impl Into<String>) {
        self.lines.push(Line {
            status,
            item: item.into(),
            detail: detail.into(),
        });
    }

    fn step(&mut self, name: impl Into<String>, actions: Vec<Action>) {
        self.steps.push(Step {
            name: name.into(),
            actions,
        });
    }

    pub fn has_changes(&self) -> bool {
        !self.steps.is_empty()
    }

    pub fn render(&self) -> String {
        let width = self
            .lines
            .iter()
            .map(|l| l.status.word().len())
            .max()
            .unwrap_or(0);
        let item_width = self.lines.iter().map(|l| l.item.len()).max().unwrap_or(0);
        self.lines
            .iter()
            .map(|l| {
                format!(
                    "  {:width$}  {:item_width$}  {}\n",
                    l.status.word(),
                    l.item,
                    l.shown_detail()
                )
                .replace(" \n", "\n")
            })
            .collect()
    }
}

fn read_optional(path: &Path) -> Result<Option<String>, String> {
    // Reading a FIFO would block, and a directory's error says little.
    if std::fs::metadata(path).is_ok_and(|m| !m.is_file()) {
        return Err(format!("{} is not a regular file", path.display()));
    }
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        // A file where a folder on the path should be: nothing is there to
        // read, and `check_writable` names the culprit.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn accessible(path: &Path, mode: libc::c_int) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: c_path is a valid NUL-terminated string for the call's duration.
    unsafe { libc::access(c_path.as_ptr(), mode) == 0 }
}

/// Whether a write to `path` will succeed, asked while planning: a
/// directory or a dangling link where the file goes, or a folder the user
/// cannot write, would otherwise fail halfway through the apply, after
/// earlier steps were written.
fn check_writable(path: &Path) -> Result<(), String> {
    if std::fs::symlink_metadata(path).is_ok() {
        let meta = std::fs::metadata(path)
            .map_err(|_| format!("{} is a link to nothing", path.display()))?;
        if !meta.is_file() {
            return Err(format!("{} is not a regular file", path.display()));
        }
        if !accessible(path, libc::W_OK) {
            return Err(format!("{} is not writable", path.display()));
        }
        return Ok(());
    }
    // Missing: the folders up to the first one that exists get created.
    let mut dir = path.parent();
    while let Some(d) = dir {
        if std::fs::symlink_metadata(d).is_ok() {
            if !std::fs::metadata(d).is_ok_and(|m| m.is_dir()) {
                return Err(format!("{} is not a directory", d.display()));
            }
            if !accessible(d, libc::W_OK | libc::X_OK) {
                return Err(format!("{} is not writable", d.display()));
            }
            return Ok(());
        }
        dir = d.parent();
    }
    Err(format!("no existing folder above {}", path.display()))
}

/// Whether `path` can be removed: its folder must be writable.
fn check_removable(path: &Path) -> Result<(), String> {
    match path.parent() {
        Some(dir) if !accessible(dir, libc::W_OK | libc::X_OK) => {
            Err(format!("{} is not writable", dir.display()))
        }
        _ => Ok(()),
    }
}

/// The whole plan. An error here means nothing has been touched.
pub fn plan(ctx: &Context, direction: Direction, sys: &mut dyn System) -> Result<Plan, String> {
    let mut plan = Plan::default();
    plan_mod(ctx, direction, &mut plan)?;
    plan_keys(ctx, direction, &mut plan)?;
    plan_link(ctx, direction, &mut plan);
    plan_service(ctx, direction, sys, &mut plan)?;
    Ok(plan)
}

fn plan_mod(ctx: &Context, direction: Direction, plan: &mut Plan) -> Result<(), String> {
    let dir = ctx.mod_dir();
    for (name, contents) in MOD_FILES {
        let path = dir.join(name);
        let item = format!("mod {name}");
        let detail = ctx.show(&path);
        let existing = read_optional(&path)?;
        match (direction, existing) {
            (Direction::Install, None) => {
                check_writable(&path)?;
                plan.line(Status::Create, &item, detail);
                plan.step(&item, vec![Action::Write(path, contents.to_string())]);
            }
            (Direction::Install, Some(text)) if text == contents => {
                plan.line(Status::Unchanged, &item, detail)
            }
            (Direction::Install, Some(_)) => {
                check_writable(&path)?;
                plan.line(
                    Status::Update,
                    &item,
                    format!("{detail}; local edits are replaced"),
                );
                plan.step(&item, vec![Action::Write(path, contents.to_string())]);
            }
            (Direction::Uninstall, None) => plan.line(Status::Unchanged, &item, "not installed"),
            (Direction::Uninstall, Some(text)) => {
                check_removable(&path)?;
                let detail = if text == contents {
                    detail
                } else {
                    format!("{detail}; local edits are removed")
                };
                plan.line(Status::Remove, &item, detail);
                plan.step(&item, vec![Action::Remove(path)]);
            }
        }
    }
    if direction == Direction::Uninstall
        && std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir())
    {
        plan_mod_dir(ctx, &dir, plan)?;
    }
    Ok(())
}

/// Files herdr-app writes into every mod folder it loads. They are its,
/// not the user's, so uninstall takes them too; otherwise the folder never
/// ends up empty.
const APP_FILES: [&str; 2] = ["gpui-kit.d.ts", "jsconfig.json"];

/// Uninstall: the app's files in the mod folder, then the folder if nothing
/// else is left. Anything else there is the user's and keeps it.
fn plan_mod_dir(ctx: &Context, dir: &Path, plan: &mut Plan) -> Result<(), String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| !MOD_FILES.iter().any(|(ours, _)| ours == name))
        .collect();
    names.sort();
    let mut kept = Vec::new();
    for name in names {
        let path = dir.join(&name);
        // A link is the user's doing, whatever its name.
        let app_file = APP_FILES.contains(&name.as_str())
            && std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file());
        if !app_file {
            kept.push(name);
            continue;
        }
        check_removable(&path)?;
        let item = format!("mod {name}");
        plan.line(
            Status::Remove,
            &item,
            format!("{}; written by herdr-app", ctx.show(&path)),
        );
        plan.step(&item, vec![Action::Remove(path)]);
    }
    if kept.is_empty() {
        check_removable(dir)?;
        plan.line(Status::Remove, "mod directory", ctx.show(dir));
        // Guarded again when applied: the folder goes only if empty.
        plan.step(
            "mod directory",
            vec![Action::RemoveDirIfEmpty(dir.to_path_buf())],
        );
    } else {
        plan.line(
            Status::Skip(format!("kept: it also holds {}", kept.join(", "))),
            "mod directory",
            ctx.show(dir),
        );
    }
    Ok(())
}

fn plan_keys(ctx: &Context, direction: Direction, plan: &mut Plan) -> Result<(), String> {
    let path = ctx.keys_path();
    let ours = keys::shipped(KEYS)?;
    let verb = match direction {
        Direction::Install => "install",
        Direction::Uninstall => "uninstall",
    };
    // A broken keys.json is normal while the user edits it (the app keeps
    // its last good keymap and writes keys.error), so it skips the keys and
    // nothing else.
    let skip_all = |plan: &mut Plan, why: String| {
        for (key, _) in &ours {
            plan.line(
                Status::Skip(format!("{why}: fix it and run {verb} again")),
                format!("key {key}"),
                ctx.show(&path),
            );
        }
    };
    let existing = match read_optional(&path) {
        Ok(None) if std::fs::symlink_metadata(&path).is_ok() => {
            skip_all(plan, format!("{} is a link to nothing", path.display()));
            return Ok(());
        }
        Ok(existing) => existing,
        Err(why) => {
            skip_all(plan, why);
            return Ok(());
        }
    };
    let planned = match (direction, existing.as_deref()) {
        (Direction::Install, None) => {
            // The app writes its keymap on first run; one made here would
            // hold only these two keys and none of the app's defaults.
            for (key, _) in &ours {
                plan.line(
                    Status::Skip(
                        "keys.json does not exist yet: open the Herdr app once, then run install again"
                            .to_string(),
                    ),
                    format!("key {key}"),
                    ctx.show(&path),
                );
            }
            return Ok(());
        }
        (Direction::Install, Some(text)) => keys::plan_install(text, &ours),
        (Direction::Uninstall, existing) => keys::plan_uninstall(existing, &ours),
    };
    let keys_plan = match planned {
        Ok(keys_plan) => keys_plan,
        Err(why) => {
            skip_all(plan, why);
            return Ok(());
        }
    };
    if let keys::Outcome::Write(_) = keys_plan.outcome {
        if let Err(why) = check_writable(&path) {
            skip_all(plan, why);
            return Ok(());
        }
    }
    let mut warned = false;
    for (key, status) in keys_plan.keys {
        let (status, detail) = match status {
            // Said once: the first edit is what makes the file the user's.
            keys::KeyStatus::Add if !warned => {
                warned = true;
                (
                    Status::Create,
                    format!(
                        "{}; the app then stops adding new default shortcuts to it",
                        ctx.show(&path)
                    ),
                )
            }
            keys::KeyStatus::Add => (Status::Create, ctx.show(&path)),
            keys::KeyStatus::Unchanged => (Status::Unchanged, ctx.show(&path)),
            keys::KeyStatus::Conflict(action) => {
                (Status::Skip(format!("bound to {action}")), ctx.show(&path))
            }
            keys::KeyStatus::Shadowed { action, section } => (
                Status::Skip(format!(
                    "bound to {action} in {section}, which takes precedence"
                )),
                ctx.show(&path),
            ),
            keys::KeyStatus::Remove => (Status::Remove, ctx.show(&path)),
            keys::KeyStatus::Absent => (Status::Unchanged, "not bound to this mod".to_string()),
        };
        plan.line(status, format!("key {key}"), detail);
    }
    if let keys::Outcome::Write(text) = keys_plan.outcome {
        plan.step("keys.json", vec![Action::Write(path, text)]);
    }
    Ok(())
}

fn plan_link(ctx: &Context, direction: Direction, plan: &mut Plan) {
    let link = &ctx.config_link;
    let target = &ctx.tui_config;
    let item = "config link";
    let detail = format!("{} -> {}", ctx.show(link), ctx.show(target));
    let ours = std::fs::read_link(link).is_ok_and(|at| at == *target);
    match direction {
        Direction::Install if ours => plan.line(Status::Unchanged, item, detail),
        Direction::Install if std::fs::symlink_metadata(link).is_ok() => {
            plan.line(Status::Skip(format!("{} exists", ctx.show(link))), item, "")
        }
        Direction::Install if !target.is_dir() => plan.line(
            Status::Skip(format!("no TUI plugin config at {}", ctx.show(target))),
            item,
            "",
        ),
        Direction::Install => {
            if let Err(why) = check_writable(link) {
                plan.line(Status::Skip(why), item, detail);
                return;
            }
            plan.line(Status::Create, item, detail);
            plan.step(
                item,
                vec![Action::Symlink {
                    link: link.clone(),
                    target: target.clone(),
                }],
            );
        }
        Direction::Uninstall if ours => plan.line(
            Status::Skip("left in place: it points at your rules".to_string()),
            item,
            detail,
        ),
        Direction::Uninstall => plan.line(Status::Unchanged, item, "not created by install"),
    }
}

fn plan_service(
    ctx: &Context,
    direction: Direction,
    sys: &mut dyn System,
    plan: &mut Plan,
) -> Result<(), String> {
    let Some(manager) = ctx.manager else {
        plan.line(
            Status::Skip("no supported service manager on this platform".to_string()),
            "service",
            "",
        );
        return Ok(());
    };
    let item = format!("service ({})", manager.name());
    let found = match service::locate(manager, &ctx.home, sys) {
        Ok(found) => found,
        Err(service::Unmanaged::NoSystemd(why)) => {
            return plan_without_systemd(ctx, direction, &item, &why, plan)
        }
        Err(service::Unmanaged::Unreachable(why)) => {
            plan_unreachable(ctx, direction, &item, &why, plan);
            return Ok(());
        }
    };
    let path = found.path;
    let mut detail = ctx.show(&path);
    if direction == Direction::Install {
        // A build tree's binary would pin the service to that tree.
        detail = format!("{detail}; runs {}", ctx.show(&ctx.bin));
    }
    if let Some(note) = found.note {
        detail = format!("{detail}; {note}");
    }
    let existing = read_optional(&path)?;
    if direction == Direction::Uninstall {
        if existing.is_none() {
            plan.line(Status::Unchanged, item, "not installed");
            return Ok(());
        }
        check_removable(&path)?;
        plan.line(Status::Remove, &item, detail);
        let actions = match manager {
            Manager::Systemd => vec![
                Action::Run(service::disable_now()),
                Action::Remove(path),
                Action::Run(service::daemon_reload()),
            ],
            Manager::Launchd => vec![Action::BootoutIfLoaded(ctx.uid), Action::Remove(path)],
        };
        plan.step(&item, actions);
        return Ok(());
    }

    let contents = service::render(manager, &ctx.bin, &ctx.env);
    let status = match &existing {
        None => Status::Create,
        Some(text) if *text == contents => manager_state(manager, ctx.uid, sys),
        Some(_) => Status::Update,
    };
    if matches!(status, Status::Create | Status::Update) {
        check_writable(&path)?;
    }
    // A daemon the service did not start holds the lock, and the service
    // would only wait behind it.
    if let Some(pid) = sys.foreign_daemon() {
        plan.line(
            Status::Stop,
            "running daemon",
            format!("pid {pid}, not started by the service"),
        );
        let mut actions = vec![Action::StopDaemon(pid)];
        if status == Status::Unchanged {
            actions.push(Action::Run(service::start_cmd(manager, ctx.uid)));
        }
        plan.step("stop the running daemon", actions);
    }
    plan.line(status.clone(), &item, detail);
    let actions = match (manager, &status) {
        (_, Status::Unchanged) => return Ok(()),
        (_, Status::Start) => vec![Action::Run(service::start_cmd(manager, ctx.uid))],
        (Manager::Systemd, Status::Enable) => vec![
            Action::Run(service::daemon_reload()),
            Action::Run(service::enable_now()),
        ],
        (Manager::Systemd, Status::Create) => vec![
            Action::Write(path, contents),
            Action::Run(service::daemon_reload()),
            Action::Run(service::enable_now()),
        ],
        (Manager::Systemd, _) => vec![
            Action::Write(path, contents),
            Action::Run(service::daemon_reload()),
            Action::Run(service::enable_now()),
            Action::Run(service::restart()),
        ],
        (Manager::Launchd, Status::Enable) => vec![Action::Run(service::bootstrap(ctx.uid, &path))],
        (Manager::Launchd, _) => vec![
            Action::BootoutIfLoaded(ctx.uid),
            Action::Write(path.clone(), contents),
            Action::Run(service::bootstrap(ctx.uid, &path)),
        ],
    };
    plan.step(&item, actions);
    Ok(())
}

/// systemd does not run here, so there is no service to manage: install
/// leaves it out (the daemon still runs detached with `start`), and
/// uninstall only removes a unit file it finds where install would have put
/// it, since disable and daemon-reload have nobody to talk to.
fn plan_without_systemd(
    ctx: &Context,
    direction: Direction,
    item: &str,
    why: &str,
    plan: &mut Plan,
) -> Result<(), String> {
    if direction == Direction::Install {
        plan.line(
            Status::Skip(format!(
                "no systemd user manager answers ({why}); \
                 start the daemon with herdr-triggersd --host core start"
            )),
            item,
            "",
        );
        return Ok(());
    }
    let path = service::default_unit_path(&ctx.home);
    if read_optional(&path)?.is_none() {
        plan.line(Status::Unchanged, item, "not installed");
        return Ok(());
    }
    check_removable(&path)?;
    plan.line(
        Status::Remove,
        item,
        format!(
            "{}; no systemd user manager answers ({why}), so the file is only removed",
            ctx.show(&path)
        ),
    );
    plan.step(item, vec![Action::Remove(path)]);
    Ok(())
}

/// systemd runs, but this session cannot reach the user manager. A service
/// may be enabled and running, so nothing about it changes from here: not
/// the unit (removing it would leave the daemon running and its enablement
/// link dangling), and not a daemon install would otherwise stop for it.
fn plan_unreachable(ctx: &Context, direction: Direction, item: &str, why: &str, plan: &mut Plan) {
    let (what, detail) = match direction {
        Direction::Install => ("run install from your desktop session", String::new()),
        Direction::Uninstall => (
            "run uninstall from your desktop session to stop and remove the service",
            ctx.show(&service::default_unit_path(&ctx.home)),
        ),
    };
    plan.line(
        Status::Skip(format!(
            "cannot reach the systemd user manager from this session ({why}); {what}"
        )),
        item,
        detail,
    );
}

/// For a service file already in place: whether the manager has it enabled
/// and running. The file alone says nothing once an enable or bootstrap has
/// failed, and a re-run of install has to finish that job. launchd keeps a
/// job loaded after its daemon exits 0 (a Stop), and `print` exits 0 either
/// way, so whether it runs comes from the daemon's own pidfile and mode.
fn manager_state(manager: Manager, uid: u32, sys: &mut dyn System) -> Status {
    match manager {
        Manager::Systemd if !sys.succeeds(&service::enabled_probe()) => Status::Enable,
        Manager::Systemd if !sys.succeeds(&service::active_probe()) => Status::Start,
        Manager::Launchd if !sys.succeeds(&service::loaded_probe(uid)) => Status::Enable,
        Manager::Launchd if !sys.service_daemon_running() => Status::Start,
        _ => Status::Unchanged,
    }
}

fn apply_action(action: &Action, sys: &mut dyn System) -> Result<(), String> {
    match action {
        Action::Write(path, contents) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
            std::fs::write(path, contents)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))
        }
        Action::Remove(path) => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
        },
        Action::RemoveDirIfEmpty(dir) => {
            let empty = std::fs::read_dir(dir).map(|mut d| d.next().is_none());
            match empty {
                Ok(true) => std::fs::remove_dir(dir)
                    .map_err(|e| format!("cannot remove {}: {e}", dir.display())),
                Ok(false) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("cannot read {}: {e}", dir.display())),
            }
        }
        Action::Symlink { link, target } => {
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
            std::os::unix::fs::symlink(target, link)
                .map_err(|e| format!("cannot link {}: {e}", link.display()))
        }
        Action::Run(cmd) => sys.run(cmd),
        Action::BootoutIfLoaded(uid) => {
            if sys.succeeds(&service::loaded_probe(*uid)) {
                sys.run(&service::bootout(*uid))
            } else {
                Ok(())
            }
        }
        Action::StopDaemon(pid) => sys.stop_daemon(*pid),
    }
}

/// Applies every step in order, stopping at the first failure.
pub fn apply(plan: &Plan, sys: &mut dyn System) -> Result<(), String> {
    for step in &plan.steps {
        for action in &step.actions {
            apply_action(action, sys).map_err(|e| format!("{}: {e}", step.name))?;
        }
    }
    Ok(())
}

/// Refuses a relative directory variable. At run time a relative
/// XDG_CONFIG_HOME is ignored and the others resolve against whatever the
/// working directory is, and the service gets none of them; install would
/// link and check paths that neither it nor the CLI uses later.
pub fn absolute_dirs(get: impl Fn(&str) -> Option<String>) -> Result<(), String> {
    for name in service::PASSED_ENV {
        if let Some(value) = get(name).filter(|value| !value.is_empty()) {
            if !Path::new(&value).is_absolute() {
                return Err(format!("{name} must be absolute, not {value}"));
            }
        }
    }
    Ok(())
}

/// Refuses root acting on someone else's HOME (`sudo` keeps HOME on many
/// systems): every file would land in that home owned by root, and the
/// service would go to root's manager. `home_owner` is None when HOME cannot
/// be read.
pub fn check_user(euid: u32, home_owner: Option<u32>) -> Result<(), String> {
    if euid == 0 && home_owner != Some(0) {
        return Err("run install as your user, not with sudo".to_string());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Ask,
    Yes,
    DryRun,
}

pub fn parse_mode(args: &[String]) -> Result<Mode, String> {
    let mut mode = Mode::Ask;
    for arg in args {
        let next = match arg.as_str() {
            "--yes" | "-y" => Mode::Yes,
            "--dry-run" => Mode::DryRun,
            other => return Err(format!("unknown option {other:?}")),
        };
        if mode != Mode::Ask && mode != next {
            return Err("--yes and --dry-run cannot be combined".to_string());
        }
        mode = next;
    }
    Ok(mode)
}

/// `y` or `yes`, any case; anything else, including an empty line, is no.
pub fn confirmed(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Where `execute` talks to the person running it. The plan goes to `out`
/// and the question to `err`, so `install | less` or `> plan.txt` still
/// shows the prompt that is waiting for an answer.
pub struct Terminal<'a> {
    /// Whether stdin and stderr are both terminals, so the question is seen
    /// and can be answered.
    pub tty: bool,
    pub input: &'a mut dyn BufRead,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
}

/// Prints the plan and applies it as `mode` and the terminal allow. The exit
/// code: 0 done or nothing to do, 1 a failure or a "no", 2 no terminal to
/// ask on.
pub fn execute(
    plan: &Plan,
    direction: Direction,
    mode: Mode,
    term: Terminal<'_>,
    sys: &mut dyn System,
) -> i32 {
    let Terminal {
        tty,
        input,
        out,
        err,
    } = term;
    let verb = match direction {
        Direction::Install => "install",
        Direction::Uninstall => "uninstall",
    };
    let _ = writeln!(out, "herdr-triggers {verb}:");
    let _ = write!(out, "{}", plan.render());
    if !plan.has_changes() {
        let _ = writeln!(out, "nothing to do");
        return 0;
    }
    match mode {
        Mode::DryRun => return 0,
        Mode::Yes => {}
        Mode::Ask if !tty => {
            let _ = writeln!(err, "not a terminal: pass --yes to apply");
            return 2;
        }
        Mode::Ask => {
            let _ = out.flush();
            let _ = write!(err, "Apply? [y/N] ");
            let _ = err.flush();
            let mut answer = String::new();
            if input.read_line(&mut answer).is_err() || !confirmed(&answer) {
                let _ = writeln!(out, "nothing applied");
                return 1;
            }
        }
    }
    match apply(plan, sys) {
        Ok(()) => {
            let _ = writeln!(out, "done");
            0
        }
        Err(message) => {
            let _ = writeln!(err, "error: {message}");
            let _ = writeln!(
                err,
                "steps before it stay applied; run {verb} again to finish"
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        ran: Vec<String>,
        /// Every probe succeeds, except one naming a word in `down`.
        loaded: bool,
        down: Vec<&'static str>,
        /// What the `UnitPath` query prints, unless `query_fails`.
        unit_paths: String,
        query_fails: bool,
        /// Whether /run/systemd/system is there, for a failed query.
        booted: bool,
        foreign: Option<i32>,
        service_running: bool,
        stopped: Vec<i32>,
        fail: Option<String>,
    }

    impl System for Recorder {
        fn run(&mut self, cmd: &Cmd) -> Result<(), String> {
            self.ran.push(cmd.to_string());
            match &self.fail {
                Some(word) if cmd.to_string().contains(word.as_str()) => {
                    Err(format!("{cmd} failed"))
                }
                _ => Ok(()),
            }
        }
        fn succeeds(&mut self, cmd: &Cmd) -> bool {
            self.ran.push(format!("probe {cmd}"));
            self.loaded && !cmd.args.iter().any(|arg| self.down.contains(&arg.as_str()))
        }
        fn output(&mut self, cmd: &Cmd) -> Result<String, String> {
            assert_eq!(cmd.to_string(), service::unit_path_query().to_string());
            if self.query_fails {
                return Err("Failed to connect to bus: No medium found".to_string());
            }
            if self.unit_paths.is_empty() {
                return Ok(r#"{"type":"as","data":[]}"#.to_string());
            }
            Ok(self.unit_paths.clone())
        }
        fn systemd_booted(&mut self) -> bool {
            self.booted
        }
        fn foreign_daemon(&mut self) -> Option<i32> {
            self.foreign
        }
        fn service_daemon_running(&mut self) -> bool {
            self.service_running
        }
        fn stop_daemon(&mut self, pid: i32) -> Result<(), String> {
            self.stopped.push(pid);
            self.foreign = None;
            Ok(())
        }
    }

    struct Home {
        root: PathBuf,
        ctx: Context,
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn home(tag: &str, manager: Manager) -> Home {
        let root =
            std::env::temp_dir().join(format!("triggers-install-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let config_home = root.join(".config");
        let ctx = Context {
            home: root.clone(),
            app_home: config_home.join("herdr-app"),
            config_link: config_home.join("herdr-triggers"),
            tui_config: config_home.join("herdr/plugins/config/cantona.herdr-triggers"),
            manager: Some(manager),
            bin: PathBuf::from("/opt/bin/herdr-triggersd"),
            env: vec![("PATH".into(), "/opt/bin:/usr/bin".into())],
            uid: 501,
        };
        Home { root, ctx }
    }

    const EMPTY_KEYS: &str = "{\n  \"global\": {}\n}\n";

    /// The keys.json the app writes on its first run.
    fn app_ran_once(h: &Home) {
        std::fs::create_dir_all(&h.ctx.app_home).unwrap();
        std::fs::write(h.ctx.keys_path(), EMPTY_KEYS).unwrap();
    }

    fn statuses(plan: &Plan) -> Vec<String> {
        plan.lines
            .iter()
            .map(|l| format!("{} {}", l.status, l.item))
            .collect()
    }

    fn run(ctx: &Context, direction: Direction, sys: &mut Recorder) -> Plan {
        let plan = plan(ctx, direction, sys).unwrap();
        apply(&plan, sys).unwrap();
        plan
    }

    #[test]
    fn install_then_install_again_is_all_unchanged() {
        let h = home("twice", Manager::Systemd);
        app_ran_once(&h);
        std::fs::create_dir_all(&h.ctx.tui_config).unwrap();
        let mut sys = Recorder::default();
        let first = run(&h.ctx, Direction::Install, &mut sys);
        assert_eq!(
            statuses(&first),
            [
                "create mod main.js",
                "create mod mod.json",
                "create key ctrl-alt-g",
                "create key cmd-alt-g",
                "create config link",
                "create service (systemd)",
            ]
        );
        assert_eq!(
            sys.ran,
            [
                "systemctl --user daemon-reload",
                "systemctl --user enable --now herdr-triggers.service"
            ]
        );
        let mod_dir = h.ctx.app_home.join("mods/herdr-triggers");
        assert_eq!(
            std::fs::read_to_string(mod_dir.join("main.js")).unwrap(),
            MOD_FILES[0].1
        );
        assert!(!mod_dir.join("gpui-kit.d.ts").exists());
        assert_eq!(
            std::fs::read_link(&h.ctx.config_link).unwrap(),
            h.ctx.tui_config
        );
        let unit =
            std::fs::read_to_string(h.root.join(".config/systemd/user/herdr-triggers.service"))
                .unwrap();
        assert!(unit.contains("ExecStart=/opt/bin/herdr-triggersd --host core service"));

        let mut again = Recorder {
            loaded: true,
            ..Default::default()
        };
        let second = plan(&h.ctx, Direction::Install, &mut again).unwrap();
        assert!(!second.has_changes(), "{}", second.render());
        assert!(
            second.lines.iter().all(|l| l.status == Status::Unchanged),
            "{}",
            second.render()
        );
        assert_eq!(
            again.ran,
            [
                "probe systemctl --user is-enabled --quiet herdr-triggers.service",
                "probe systemctl --user is-active --quiet herdr-triggers.service"
            ]
        );

        std::fs::write(mod_dir.join("main.js"), "// edited").unwrap();
        let edited = plan(&h.ctx, Direction::Install, &mut again).unwrap();
        assert_eq!(statuses(&edited)[0], "update mod main.js");
        apply(&edited, &mut again).unwrap();
        assert_eq!(
            std::fs::read_to_string(mod_dir.join("main.js")).unwrap(),
            MOD_FILES[0].1
        );
    }

    #[test]
    fn uninstall_after_install_keeps_the_keys_file_and_the_link() {
        let h = home("restore", Manager::Systemd);
        app_ran_once(&h);
        std::fs::create_dir_all(&h.ctx.tui_config).unwrap();
        let mut sys = Recorder::default();
        run(&h.ctx, Direction::Install, &mut sys);
        let mut sys = Recorder::default();
        let undo = run(&h.ctx, Direction::Uninstall, &mut sys);
        assert_eq!(
            statuses(&undo),
            [
                "remove mod main.js",
                "remove mod mod.json",
                "remove mod directory",
                "remove key ctrl-alt-g",
                "remove key cmd-alt-g",
                "skip: left in place: it points at your rules config link",
                "remove service (systemd)",
            ]
        );
        assert_eq!(
            sys.ran,
            [
                "systemctl --user disable --now herdr-triggers.service",
                "systemctl --user daemon-reload"
            ]
        );
        assert_eq!(
            std::fs::read_to_string(h.ctx.keys_path()).unwrap(),
            EMPTY_KEYS
        );
        assert!(!h.ctx.app_home.join("mods/herdr-triggers").exists());
        assert!(h.ctx.app_home.join("mods").is_dir());
        assert!(std::fs::read_link(&h.ctx.config_link).is_ok());
        assert!(!h
            .root
            .join(".config/systemd/user/herdr-triggers.service")
            .exists());
    }

    #[test]
    fn uninstall_keeps_the_users_keys_and_other_mod_files() {
        let h = home("users", Manager::Launchd);
        let before = "{\n  \"global\": {\n    \"alt-1\": \"space.select:1\",\n    \"cmd-alt-g\": \"tab.new\"\n  }\n}\n";
        std::fs::create_dir_all(&h.ctx.app_home).unwrap();
        std::fs::write(h.ctx.keys_path(), before).unwrap();
        let mut sys = Recorder::default();
        let plan_in = run(&h.ctx, Direction::Install, &mut sys);
        assert!(statuses(&plan_in).contains(&"skip: bound to tab.new key cmd-alt-g".to_string()));
        std::fs::write(h.ctx.mod_dir().join("notes.txt"), "mine").unwrap();
        std::fs::write(h.ctx.mod_dir().join("jsconfig.json"), "{}").unwrap();
        let mut sys = Recorder::default();
        let undo = run(&h.ctx, Direction::Uninstall, &mut sys);
        assert_eq!(std::fs::read_to_string(h.ctx.keys_path()).unwrap(), before);
        assert!(h.ctx.mod_dir().join("notes.txt").exists());
        assert!(!h.ctx.mod_dir().join("main.js").exists());
        assert!(!h.ctx.mod_dir().join("jsconfig.json").exists());
        let kept = undo
            .lines
            .iter()
            .find(|l| l.item == "mod directory")
            .unwrap();
        assert_eq!(
            kept.status,
            Status::Skip("kept: it also holds notes.txt".into())
        );
    }

    #[test]
    fn uninstall_takes_the_apps_files_and_then_the_folder() {
        let h = home("appfiles", Manager::Systemd);
        app_ran_once(&h);
        let mut sys = Recorder::default();
        run(&h.ctx, Direction::Install, &mut sys);
        let dir = h.ctx.mod_dir();
        std::fs::write(dir.join("gpui-kit.d.ts"), "declare module").unwrap();
        std::fs::write(dir.join("jsconfig.json"), "{}").unwrap();
        let mut sys = Recorder::default();
        let undo = run(&h.ctx, Direction::Uninstall, &mut sys);
        let mod_lines: Vec<String> = statuses(&undo)
            .into_iter()
            .filter(|l| l.contains(" mod "))
            .collect();
        assert_eq!(
            mod_lines,
            [
                "remove mod main.js",
                "remove mod mod.json",
                "remove mod gpui-kit.d.ts",
                "remove mod jsconfig.json",
                "remove mod directory"
            ]
        );
        assert!(!dir.exists());
        assert!(h.ctx.app_home.join("mods").is_dir());

        // A link with the app's file name is the user's: it keeps the folder.
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink("/elsewhere", dir.join("jsconfig.json")).unwrap();
        let mut sys = Recorder::default();
        let again = run(&h.ctx, Direction::Uninstall, &mut sys);
        assert!(statuses(&again)
            .contains(&"skip: kept: it also holds jsconfig.json mod directory".to_string()));
        assert!(std::fs::symlink_metadata(dir.join("jsconfig.json")).is_ok());
    }

    #[test]
    fn launchd_install_bootstraps_and_an_update_boots_out_first() {
        let h = home("launchd", Manager::Launchd);
        let mut sys = Recorder::default();
        run(&h.ctx, Direction::Install, &mut sys);
        let plist = h
            .root
            .join("Library/LaunchAgents/io.github.cantona.herdr-triggers.plist");
        assert!(plist.exists());
        assert_eq!(
            sys.ran,
            [
                "probe launchctl print gui/501/io.github.cantona.herdr-triggers".to_string(),
                format!("launchctl bootstrap gui/501 {}", plist.display()),
            ]
        );

        let mut moved = h.ctx.clone();
        moved.bin = PathBuf::from("/new/bin/herdr-triggersd");
        let mut sys = Recorder {
            loaded: true,
            ..Default::default()
        };
        let plan = run(&moved, Direction::Install, &mut sys);
        assert!(statuses(&plan).contains(&"update service (launchd)".to_string()));
        assert_eq!(
            sys.ran,
            [
                "probe launchctl print gui/501/io.github.cantona.herdr-triggers".to_string(),
                "launchctl bootout gui/501/io.github.cantona.herdr-triggers".to_string(),
                format!("launchctl bootstrap gui/501 {}", plist.display()),
            ]
        );
        assert!(std::fs::read_to_string(&plist)
            .unwrap()
            .contains("/new/bin/herdr-triggersd"));
    }

    #[test]
    fn a_daemon_the_service_did_not_start_is_stopped_before_enabling() {
        let h = home("foreign", Manager::Systemd);
        let mut sys = Recorder {
            foreign: Some(4242),
            ..Default::default()
        };
        let plan = run(&h.ctx, Direction::Install, &mut sys);
        assert!(statuses(&plan).contains(&"stop running daemon".to_string()));
        assert_eq!(sys.stopped, [4242]);

        // Service already in place: stopping the stray daemon starts it.
        let mut sys = Recorder {
            foreign: Some(4343),
            loaded: true,
            ..Default::default()
        };
        run(&h.ctx, Direction::Install, &mut sys);
        assert_eq!(sys.stopped, [4343]);
        assert_eq!(
            sys.ran,
            [
                "probe systemctl --user is-enabled --quiet herdr-triggers.service",
                "probe systemctl --user is-active --quiet herdr-triggers.service",
                "systemctl --user start herdr-triggers.service"
            ]
        );
    }

    #[test]
    fn a_rerun_finishes_a_systemd_enable_that_failed() {
        let h = home("reenable", Manager::Systemd);
        let mut sys = Recorder {
            fail: Some("enable".into()),
            ..Default::default()
        };
        let first = plan(&h.ctx, Direction::Install, &mut sys).unwrap();
        assert!(apply(&first, &mut sys).is_err());

        let mut sys = Recorder {
            loaded: true,
            down: vec!["is-enabled"],
            ..Default::default()
        };
        let second = run(&h.ctx, Direction::Install, &mut sys);
        assert!(statuses(&second).contains(&"enable service (systemd)".to_string()));
        assert_eq!(
            sys.ran,
            [
                "probe systemctl --user is-enabled --quiet herdr-triggers.service",
                "systemctl --user daemon-reload",
                "systemctl --user enable --now herdr-triggers.service"
            ]
        );

        let mut sys = Recorder {
            loaded: true,
            down: vec!["is-active"],
            ..Default::default()
        };
        let stopped = run(&h.ctx, Direction::Install, &mut sys);
        assert!(statuses(&stopped).contains(&"start service (systemd)".to_string()));
        assert_eq!(
            sys.ran[2..],
            ["systemctl --user start herdr-triggers.service"]
        );
    }

    #[test]
    fn a_rerun_bootstraps_a_launchd_job_that_is_not_loaded() {
        let h = home("reload", Manager::Launchd);
        let plist = h
            .root
            .join("Library/LaunchAgents/io.github.cantona.herdr-triggers.plist");
        let mut sys = Recorder {
            fail: Some("bootstrap".into()),
            ..Default::default()
        };
        let first = plan(&h.ctx, Direction::Install, &mut sys).unwrap();
        assert!(apply(&first, &mut sys).is_err());
        assert!(plist.exists());

        let mut sys = Recorder::default();
        let second = run(&h.ctx, Direction::Install, &mut sys);
        assert!(statuses(&second).contains(&"enable service (launchd)".to_string()));
        assert_eq!(
            sys.ran,
            [
                "probe launchctl print gui/501/io.github.cantona.herdr-triggers".to_string(),
                format!("launchctl bootstrap gui/501 {}", plist.display()),
            ]
        );

        let mut sys = Recorder {
            loaded: true,
            service_running: true,
            ..Default::default()
        };
        let third = plan(&h.ctx, Direction::Install, &mut sys).unwrap();
        assert!(!third.has_changes(), "{}", third.render());

        // Loaded, but its daemon was stopped: launchd does not restart a
        // clean exit, so the re-run kicks it.
        let mut sys = Recorder {
            loaded: true,
            ..Default::default()
        };
        let stopped = run(&h.ctx, Direction::Install, &mut sys);
        assert!(statuses(&stopped).contains(&"start service (launchd)".to_string()));
        assert_eq!(
            sys.ran,
            [
                "probe launchctl print gui/501/io.github.cantona.herdr-triggers",
                "launchctl kickstart gui/501/io.github.cantona.herdr-triggers"
            ]
        );
    }

    #[test]
    fn the_plan_says_local_edits_to_the_mod_are_lost() {
        let h = home("edits", Manager::Systemd);
        let mut sys = Recorder::default();
        run(&h.ctx, Direction::Install, &mut sys);
        let main_js = h.ctx.mod_dir().join("main.js");
        std::fs::write(&main_js, "// mine").unwrap();
        let detail = |plan: &Plan, item: &str| {
            plan.lines
                .iter()
                .find(|l| l.item == item)
                .map(|l| l.detail.clone())
                .unwrap()
        };
        let mut sys = Recorder::default();
        let update = plan(&h.ctx, Direction::Install, &mut sys).unwrap();
        assert!(detail(&update, "mod main.js").ends_with("main.js; local edits are replaced"));

        let undo = plan(&h.ctx, Direction::Uninstall, &mut sys).unwrap();
        assert!(detail(&undo, "mod main.js").ends_with("main.js; local edits are removed"));
        assert!(detail(&undo, "mod mod.json").ends_with("mod.json"));
    }

    #[test]
    fn a_skip_reason_goes_in_the_detail_column() {
        let mut plan = Plan::default();
        plan.line(Status::Create, "mod main.js", "~/m");
        plan.line(
            Status::Skip("/a/very/long/path/that/would/widen/every/row exists".into()),
            "config link",
            "",
        );
        plan.line(
            Status::Skip("bound to tab.new".into()),
            "key cmd-alt-g",
            "~/keys.json",
        );
        assert_eq!(
            plan.render(),
            "  create  mod main.js    ~/m\n\
             \x20 skip    config link    /a/very/long/path/that/would/widen/every/row exists\n\
             \x20 skip    key cmd-alt-g  ~/keys.json: bound to tab.new\n"
        );
    }

    #[test]
    fn the_config_link_needs_the_tui_config_and_a_free_path() {
        let h = home("link", Manager::Systemd);
        let mut sys = Recorder::default();
        let without = plan(&h.ctx, Direction::Install, &mut sys).unwrap();
        assert!(statuses(&without)
            .iter()
            .any(|s| s.starts_with("skip: no TUI plugin config at") && s.ends_with("config link")));

        std::fs::create_dir_all(&h.ctx.tui_config).unwrap();
        std::fs::create_dir_all(&h.ctx.config_link).unwrap();
        let taken = plan(&h.ctx, Direction::Install, &mut sys).unwrap();
        assert!(statuses(&taken)
            .iter()
            .any(|s| s.ends_with("exists config link")));
        apply(&taken, &mut sys).unwrap();
        assert!(h.ctx.config_link.is_dir());
        assert!(std::fs::read_link(&h.ctx.config_link).is_err());
    }

    #[test]
    fn a_broken_keys_json_skips_the_keys_and_nothing_else() {
        let h = home("invalid", Manager::Systemd);
        std::fs::create_dir_all(&h.ctx.app_home).unwrap();
        for (broken, why) in [
            ("{ not json", "keys.json is not valid JSON ("),
            ("[1]", "keys.json is not a JSON object"),
            (
                r#"{"global": 3}"#,
                "keys.json's \"global\" is not an object",
            ),
        ] {
            std::fs::write(h.ctx.keys_path(), broken).unwrap();
            for direction in [Direction::Install, Direction::Uninstall] {
                let mut sys = Recorder::default();
                let planned = run(&h.ctx, direction, &mut sys);
                let verb = if direction == Direction::Install {
                    "install"
                } else {
                    "uninstall"
                };
                for line in planned.lines.iter().filter(|l| l.item.starts_with("key ")) {
                    let Status::Skip(reason) = &line.status else {
                        panic!("{broken}: {line:?}");
                    };
                    assert!(reason.starts_with(why), "{reason}");
                    assert!(
                        reason.ends_with(&format!(": fix it and run {verb} again")),
                        "{reason}"
                    );
                }
                assert_eq!(std::fs::read_to_string(h.ctx.keys_path()).unwrap(), broken);
            }
        }
        // The other steps went ahead: the last run was an uninstall.
        assert!(!h.ctx.mod_dir().exists());
        let mut sys = Recorder::default();
        run(&h.ctx, Direction::Install, &mut sys);
        assert!(h.ctx.mod_dir().join("main.js").exists());
    }

    #[test]
    fn a_write_that_would_fail_is_found_while_planning() {
        use std::os::unix::fs::PermissionsExt;
        // The mods folder is a file: the mod cannot be written, so nothing is.
        let h = home("blocked", Manager::Systemd);
        app_ran_once(&h);
        std::fs::write(h.ctx.app_home.join("mods"), "not a folder").unwrap();
        let mut sys = Recorder::default();
        let err = plan(&h.ctx, Direction::Install, &mut sys).unwrap_err();
        assert!(err.ends_with("/herdr-app/mods is not a directory"), "{err}");

        // keys.json read-only, or a link to nothing: the keys skip, the rest goes on.
        let h = home("readonly", Manager::Systemd);
        app_ran_once(&h);
        let keys = h.ctx.keys_path();
        std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o444)).unwrap();
        let mut sys = Recorder::default();
        let planned = plan(&h.ctx, Direction::Install, &mut sys).unwrap();
        let line = planned
            .lines
            .iter()
            .find(|l| l.item == "key ctrl-alt-g")
            .unwrap();
        assert_eq!(
            line.status,
            Status::Skip(format!(
                "{} is not writable: fix it and run install again",
                keys.display()
            ))
        );
        apply(&planned, &mut sys).unwrap();
        assert!(h.ctx.mod_dir().join("main.js").exists());
        assert_eq!(std::fs::read_to_string(&keys).unwrap(), EMPTY_KEYS);

        std::fs::remove_file(&keys).unwrap();
        std::os::unix::fs::symlink(h.root.join("gone/keys.json"), &keys).unwrap();
        let planned = plan(&h.ctx, Direction::Install, &mut sys).unwrap();
        let line = planned
            .lines
            .iter()
            .find(|l| l.item == "key cmd-alt-g")
            .unwrap();
        assert_eq!(
            line.status,
            Status::Skip(format!(
                "{} is a link to nothing: fix it and run install again",
                keys.display()
            ))
        );
    }

    #[test]
    fn a_failing_step_is_named_and_earlier_steps_stay() {
        let h = home("fail", Manager::Systemd);
        app_ran_once(&h);
        let mut sys = Recorder {
            fail: Some("enable".into()),
            ..Default::default()
        };
        let plan = plan(&h.ctx, Direction::Install, &mut sys).unwrap();
        let err = apply(&plan, &mut sys).unwrap_err();
        assert!(err.starts_with("service (systemd): "), "{err}");
        assert!(h.ctx.mod_dir().join("main.js").exists());
        assert!(std::fs::read_to_string(h.ctx.keys_path())
            .unwrap()
            .contains("mod.open:herdr-triggers"));
    }

    #[test]
    fn without_keys_json_the_bindings_are_skipped_and_none_is_created() {
        let h = home("nokeys", Manager::Systemd);
        let mut sys = Recorder::default();
        let first = run(&h.ctx, Direction::Install, &mut sys);
        let keys: Vec<&Line> = first
            .lines
            .iter()
            .filter(|l| l.item.starts_with("key "))
            .collect();
        assert_eq!(keys.len(), 2);
        for line in keys {
            assert_eq!(
                line.status,
                Status::Skip(
                    "keys.json does not exist yet: open the Herdr app once, then run install again"
                        .into()
                )
            );
        }
        assert!(!h.ctx.keys_path().exists());
        assert!(h.ctx.mod_dir().join("main.js").exists());

        let mut sys = Recorder::default();
        let undo = plan(&h.ctx, Direction::Uninstall, &mut sys).unwrap();
        assert!(statuses(&undo).contains(&"unchanged key ctrl-alt-g".to_string()));
    }

    #[test]
    fn the_first_added_key_says_the_file_becomes_the_users() {
        let h = home("yours", Manager::Systemd);
        app_ran_once(&h);
        std::fs::write(
            h.ctx.keys_path(),
            r#"{"global": {}, "terminal": {"cmd-alt-g": "terminal"}}"#,
        )
        .unwrap();
        let mut sys = Recorder::default();
        let plan = plan(&h.ctx, Direction::Install, &mut sys).unwrap();
        let line = |item: &str| plan.lines.iter().find(|l| l.item == item).unwrap();
        assert!(line("key ctrl-alt-g")
            .detail
            .ends_with("keys.json; the app then stops adding new default shortcuts to it"));
        assert_eq!(
            line("key cmd-alt-g").status,
            Status::Skip("bound to terminal in terminal, which takes precedence".into())
        );
    }

    #[test]
    fn the_unit_goes_where_the_user_manager_looks() {
        let h = home("manager-env", Manager::Systemd);
        let manager_dir = h.root.join("manager-cfg/systemd/user");
        let listed = serde_json::json!({
            "type": "as",
            "data": [
                format!("{}/.config/systemd/user.control", h.root.display()),
                "/run/user/1/systemd/transient",
                manager_dir.display().to_string(),
                "/etc/systemd/user",
                format!("{}/.local/share/systemd/user", h.root.display()),
            ]
        })
        .to_string();
        let mut sys = Recorder {
            unit_paths: listed.clone(),
            ..Default::default()
        };
        run(&h.ctx, Direction::Install, &mut sys);
        let unit = manager_dir.join("herdr-triggers.service");
        assert!(unit.exists());
        assert!(!h.root.join(".config/systemd").exists());

        let mut sys = Recorder {
            unit_paths: listed,
            ..Default::default()
        };
        run(&h.ctx, Direction::Uninstall, &mut sys);
        assert!(!unit.exists());
        assert_eq!(
            sys.ran,
            [
                "systemctl --user disable --now herdr-triggers.service",
                "systemctl --user daemon-reload"
            ]
        );

        // The plan line names the binary the unit runs.
        let mut sys = Recorder {
            unit_paths: serde_json::json!({"type": "as", "data": []}).to_string(),
            ..Default::default()
        };
        let planned = plan(&h.ctx, Direction::Install, &mut sys).unwrap();
        let line = planned
            .lines
            .iter()
            .find(|l| l.item == "service (systemd)")
            .unwrap();
        assert_eq!(
            line.detail,
            "~/.config/systemd/user/herdr-triggers.service; runs /opt/bin/herdr-triggersd; \
             systemd's UnitPath lists no user config folder; assuming ~/.config/systemd/user"
        );
    }

    #[test]
    fn a_manager_out_of_reach_leaves_the_service_alone() {
        let h = home("unreachable", Manager::Systemd);
        app_ran_once(&h);
        let unit = h.root.join(".config/systemd/user/herdr-triggers.service");
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(&unit, "[Service]\n").unwrap();
        let why = "Failed to connect to bus: No medium found";
        let unreachable = || Recorder {
            query_fails: true,
            booted: true,
            foreign: Some(4242),
            ..Default::default()
        };
        let mut sys = unreachable();
        let planned = run(&h.ctx, Direction::Install, &mut sys);
        let line = planned
            .lines
            .iter()
            .find(|l| l.item == "service (systemd)")
            .unwrap();
        assert_eq!(
            line.status,
            Status::Skip(format!(
                "cannot reach the systemd user manager from this session ({why}); \
                 run install from your desktop session"
            ))
        );
        assert!(!statuses(&planned).contains(&"stop running daemon".to_string()));
        assert!(
            sys.stopped.is_empty() && sys.ran.is_empty(),
            "{:?}",
            sys.ran
        );
        assert!(h.ctx.mod_dir().join("main.js").exists());

        let mut sys = unreachable();
        let undo = run(&h.ctx, Direction::Uninstall, &mut sys);
        let line = undo
            .lines
            .iter()
            .find(|l| l.item == "service (systemd)")
            .unwrap();
        assert_eq!(
            line.status,
            Status::Skip(format!(
                "cannot reach the systemd user manager from this session ({why}); \
                 run uninstall from your desktop session to stop and remove the service"
            ))
        );
        assert!(unit.exists(), "the unit was removed");
        assert!(
            !h.ctx.mod_dir().exists(),
            "the rest of uninstall did not run"
        );

        // The plain lookup sees it as installed, but out of reach.
        let mut sys = unreachable();
        assert_eq!(
            service::installed(&h.ctx.home, &mut sys),
            if cfg!(target_os = "linux") {
                Some(service::Installed::Unreachable {
                    path: unit.clone(),
                    why: why.to_string(),
                })
            } else {
                // `installed` asks the platform's manager: launchd elsewhere.
                None
            }
        );

        // A unit in another UnitPath folder cannot be seen from here, but a
        // running daemon the service started still says it is supervised.
        std::fs::remove_file(&unit).unwrap();
        let mut sys = unreachable();
        assert_eq!(service::installed(&h.ctx.home, &mut sys), None);
        let mut sys = Recorder {
            service_running: true,
            ..unreachable()
        };
        assert_eq!(
            service::installed(&h.ctx.home, &mut sys),
            if cfg!(target_os = "linux") {
                Some(service::Installed::Unreachable {
                    path: unit.clone(),
                    why: why.to_string(),
                })
            } else {
                None
            }
        );
    }

    #[test]
    fn without_a_systemd_user_manager_there_is_no_service() {
        let h = home("no-manager", Manager::Systemd);
        app_ran_once(&h);
        let mut sys = Recorder {
            query_fails: true,
            ..Default::default()
        };
        let planned = run(&h.ctx, Direction::Install, &mut sys);
        let line = planned
            .lines
            .iter()
            .find(|l| l.item == "service (systemd)")
            .unwrap();
        assert_eq!(
            line.status,
            Status::Skip(
                "no systemd user manager answers (Failed to connect to bus: No medium found); \
                 start the daemon with herdr-triggersd --host core start"
                    .into()
            )
        );
        assert!(sys.ran.is_empty(), "{:?}", sys.ran);
        assert!(h.ctx.mod_dir().join("main.js").exists());
        let unit = h.root.join(".config/systemd/user/herdr-triggers.service");
        assert!(!unit.exists());

        // A unit file left from when a manager answered: removed, and only that.
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(&unit, "[Service]\n").unwrap();
        let mut sys = Recorder {
            query_fails: true,
            ..Default::default()
        };
        let undo = run(&h.ctx, Direction::Uninstall, &mut sys);
        let line = undo
            .lines
            .iter()
            .find(|l| l.item == "service (systemd)")
            .unwrap();
        assert_eq!(line.status, Status::Remove);
        assert!(line.detail.ends_with(
            "no systemd user manager answers (Failed to connect to bus: No medium found), \
             so the file is only removed"
        ));
        assert!(!unit.exists());
        assert!(sys.ran.is_empty(), "{:?}", sys.ran);

        let mut sys = Recorder {
            query_fails: true,
            ..Default::default()
        };
        let again = plan(&h.ctx, Direction::Uninstall, &mut sys).unwrap();
        assert!(statuses(&again).contains(&"unchanged service (systemd)".to_string()));
    }

    #[test]
    fn a_relative_directory_variable_is_refused() {
        assert!(absolute_dirs(|_| None).is_ok());
        assert!(absolute_dirs(|name| match name {
            "XDG_STATE_HOME" => Some("/s".to_string()),
            _ => Some(String::new()),
        })
        .is_ok());
        for name in service::PASSED_ENV {
            assert_eq!(
                absolute_dirs(|asked| (asked == name).then(|| "rel/dir".to_string())).unwrap_err(),
                format!("{name} must be absolute, not rel/dir")
            );
        }
    }

    #[test]
    fn root_is_refused_unless_home_is_roots() {
        assert!(check_user(1000, Some(1000)).is_ok());
        assert!(check_user(1000, None).is_ok());
        assert!(check_user(0, Some(0)).is_ok());
        assert_eq!(
            check_user(0, Some(1000)).unwrap_err(),
            "run install as your user, not with sudo"
        );
        assert!(check_user(0, None).is_err());
    }

    #[test]
    fn modes_and_answers_parse() {
        let a = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_mode(&a(&[])).unwrap(), Mode::Ask);
        assert_eq!(parse_mode(&a(&["--yes"])).unwrap(), Mode::Yes);
        assert_eq!(parse_mode(&a(&["--dry-run"])).unwrap(), Mode::DryRun);
        assert!(parse_mode(&a(&["--yes", "--dry-run"])).is_err());
        assert!(parse_mode(&a(&["--write"])).is_err());
        for yes in ["y", "Y\n", " yes ", "YES"] {
            assert!(confirmed(yes), "{yes:?}");
        }
        for no in ["", "\n", "n", "no", "yep", "ye"] {
            assert!(!confirmed(no), "{no:?}");
        }
    }

    fn exec(
        plan: &Plan,
        mode: Mode,
        tty: bool,
        answer: &str,
        sys: &mut Recorder,
    ) -> (i32, String, String) {
        let mut input = std::io::Cursor::new(answer.as_bytes().to_vec());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = execute(
            plan,
            Direction::Install,
            mode,
            Terminal {
                tty,
                input: &mut input,
                out: &mut out,
                err: &mut err,
            },
            sys,
        );
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn without_a_terminal_or_yes_nothing_is_applied_and_the_exit_is_2() {
        let h = home("notty", Manager::Systemd);
        let mut sys = Recorder::default();
        let p = plan(&h.ctx, Direction::Install, &mut sys).unwrap();
        let (code, out, err) = exec(&p, Mode::Ask, false, "y\n", &mut sys);
        assert_eq!(code, 2);
        assert!(out.contains("create"), "{out}");
        assert_eq!(err, "not a terminal: pass --yes to apply\n");
        assert!(!h.ctx.mod_dir().exists());

        let (code, _, _) = exec(&p, Mode::DryRun, false, "", &mut sys);
        assert_eq!(code, 0);
        assert!(!h.ctx.mod_dir().exists());

        let (code, out, err) = exec(&p, Mode::Ask, true, "\n", &mut sys);
        assert_eq!(code, 1);
        assert_eq!(err, "Apply? [y/N] ");
        assert!(
            !out.contains("Apply?") && out.ends_with("nothing applied\n"),
            "{out}"
        );
        assert!(!h.ctx.mod_dir().exists());

        let (code, _, _) = exec(&p, Mode::Ask, true, "y\n", &mut sys);
        assert_eq!(code, 0);
        assert!(h.ctx.mod_dir().join("main.js").exists());
    }
}
