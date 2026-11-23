//! `factory cutover` and `factory cutover --rollback` (#20): factory lab takes the `devlab`
//! name on PATH from today's TypeScript devlab, and gives it back.
//!
//! The two never lease from one PC at once. The cutover goes ahead only while today's devlab
//! holds nothing: not in its `status`, and not in its ledger, which the cutover reads under the
//! ledger's write lock while it writes the marker (`cutover.json` in devlab's state dir). From
//! that moment today's devlab refuses every call but `help`, a call already under way at its
//! next transaction. Then the watcher unit is stopped and disabled, its unit file kept (systemd's
//! `disable` deletes a linked one, so it is linked again), and `~/.local/bin/devlab`
//! becomes a link to `factory`, which answers to the name. Each step that fails undoes the ones
//! before it. The rollback, once factory lab holds nothing, puts the old link back, links the
//! watcher's unit again if it went missing, starts the watcher, and removes the marker.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use factory_adapters::devlab::{OldDevlab, TsDevlab};
use factory_adapters::systemd::{Systemctl, SystemdError, UserManager};
use factory_core::cutover::{busy, lease, named, shown};
use factory_core::exit::Exit;
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::clock;
use crate::lab;
use crate::output::{Done, Failure};
use crate::paths::Paths;

/// Today's devlab's watcher, in systemd's user unit dir.
const WATCHER: &str = "devlab-watch.service";
/// What today's devlab says once it was cut over (DotFiles' `refuseIfCutOver`).
const REFUSED: &str = "cut over to factory lab";
/// How long the cutover waits for today's devlab's write lock, as devlab itself does.
const BUSY_TIMEOUT: Duration = Duration::from_secs(15);
const UNTIL_CUT: &str = "cut over once devlab status shows no lease";
const UNTIL_ROLLED: &str = "roll back once devlab status shows no lease";

/// The marker: written by the cutover, removed by the rollback. Today's devlab refuses while it
/// is there; the rollback puts `devlab` back from it.
#[derive(Serialize, Deserialize)]
struct Marker {
    /// What `~/.local/bin/devlab` linked to before the cutover.
    devlab: PathBuf,
    factory: PathBuf,
    at: String,
    /// What the watcher's unit file was: the rollback links it again when `disable` took it.
    #[serde(default)]
    watcher: Unit,
}

/// The watcher's unit file in systemd's user unit dir, as the cutover found it.
#[derive(Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Unit {
    /// A marker from before #30 does not say.
    #[default]
    Unrecorded,
    Absent,
    /// A plain file, which `disable` leaves.
    File,
    /// A link (as stow makes), by its text, which `disable` deletes.
    Link(PathBuf),
}

impl Unit {
    fn at(path: &Path) -> Unit {
        match fs::read_link(path) {
            Ok(text) => Unit::Link(text),
            Err(_) if fs::symlink_metadata(path).is_ok() => Unit::File,
            Err(_) => Unit::Absent,
        }
    }
}

/// Where the cutover looks and writes.
struct Place {
    /// `~/.local/bin/devlab`.
    link: PathBuf,
    bin: PathBuf,
    /// `~/.local/bin/factory`, where `factory install` put it.
    factory: PathBuf,
    /// `${XDG_STATE_HOME:-~/.local/state}/devlab`, today's devlab's state dir.
    state: PathBuf,
    watcher: PathBuf,
}

impl Place {
    fn from_env() -> Result<Place, Failure> {
        let paths = Paths::from_env()?;
        Ok(Place {
            link: paths.bin.join("devlab"),
            factory: paths.bin.join("factory"),
            bin: paths.bin,
            state: lab::devlab_state(),
            watcher: paths.units.join(WATCHER),
        })
    }

    fn marker(&self) -> PathBuf {
        self.state.join("cutover.json")
    }

    fn ledger(&self) -> PathBuf {
        self.state.join("devlab.db")
    }

    /// The program a link text names, from the link's dir; canonical when it can be.
    fn program(&self, text: &Path) -> PathBuf {
        let program = self.bin.join(text);
        fs::canonicalize(&program).unwrap_or(program)
    }
}

/// What `~/.local/bin/devlab` is.
enum Link {
    /// A link to factory.
    Factory,
    /// A link to anything else: today's devlab, by this text.
    Old(PathBuf),
    Missing,
    /// A file or dir, not a link.
    Other,
}

fn link_state(place: &Place) -> Link {
    match fs::read_link(&place.link) {
        Ok(text) => {
            let real = |path: &Path| fs::canonicalize(path).ok();
            if text == Path::new("factory")
                || real(&place.link).is_some_and(|it| Some(it) == real(&place.factory))
            {
                Link::Factory
            } else {
                Link::Old(text)
            }
        }
        Err(_) if fs::symlink_metadata(&place.link).is_err() => Link::Missing,
        Err(_) => Link::Other,
    }
}

pub fn cutover() -> Result<Done, Failure> {
    let place = Place::from_env()?;
    let marked = place.marker().exists();
    let old = match link_state(&place) {
        Link::Factory if marked => {
            return Ok(Done::new(
                format!(
                    "already cut over: {} runs factory lab; roll back with factory cutover --rollback",
                    place.link.display()
                ),
                json!({"devlab": place.link, "factory": place.factory, "already": true}),
            ));
        }
        Link::Factory => {
            return Err(Failure::precondition(format!(
                "{} runs factory lab but {} is gone, so today's devlab would lease too; link {} to today's devlab, then run factory cutover again",
                place.link.display(),
                place.marker().display(),
                place.link.display()
            )));
        }
        Link::Missing => {
            return Err(Failure::precondition(format!(
                "no devlab at {} to cut over from; link today's devlab there (dots link), then run factory cutover again",
                place.link.display()
            )));
        }
        Link::Other => {
            return Err(Failure::precondition(format!(
                "{} is not a link to today's devlab; move it aside, then run factory cutover again",
                place.link.display()
            )));
        }
        Link::Old(text) => text,
    };
    if marked {
        return Err(Failure::precondition(format!(
            "{} is left from a cutover or rollback that did not finish; run factory cutover --rollback, then factory cutover again",
            place.marker().display()
        )));
    }
    if !place.factory.is_file() {
        return Err(Failure::precondition(format!(
            "no factory at {} to hand devlab to; run factory install first",
            place.factory.display()
        )));
    }
    let program = place.program(&old);
    let devlab = TsDevlab {
        program: program.clone(),
    };
    let held = status_shows(&devlab, &program)?;
    if !held.is_empty() {
        return Err(Failure::new(
            Exit::Busy,
            busy("devlab status shows", &held, UNTIL_CUT),
        ));
    }
    let unit = Unit::at(&place.watcher);
    mark_unless_held(&place, &old, &unit)?;
    let undo = Undo { place: &place };
    if !refuses(&devlab) {
        return Err(undo.marker(Failure::precondition(format!(
            "today's devlab at {} still answers once cut over; merge DotFiles' cutover refusal into the checkout it runs from, then run factory cutover again",
            program.display()
        ))));
    }
    if let Err(failure) = lab::board() {
        return Err(undo.marker(Failure::precondition(format!(
            "factory lab does not answer ({}); fix it, then run factory cutover again",
            failure.message
        ))));
    }
    let systemd = Systemctl;
    let retire = unit != Unit::Absent;
    if retire && let Err(failure) = retire_watcher(&place, &systemd, &unit) {
        return Err(undo.watcher(&systemd, failure));
    }
    if let Err(failure) = relink(&place, Path::new("factory")) {
        return Err(if retire {
            undo.watcher(&systemd, failure)
        } else {
            undo.marker(failure)
        });
    }
    Ok(Done::new(
        if retire {
            format!(
                "cut over: {} runs factory lab now; {WATCHER} is stopped and disabled; roll back with factory cutover --rollback",
                place.link.display()
            )
        } else {
            format!(
                "cut over: {} runs factory lab now; no {WATCHER} to retire; roll back with factory cutover --rollback",
                place.link.display()
            )
        },
        json!({
            "devlab": place.link,
            "factory": place.factory,
            "was": old,
            "watcher": if retire { "stopped and disabled" } else { "not installed" },
        }),
    ))
}

pub fn rollback(force: bool) -> Result<Done, Failure> {
    let place = Place::from_env()?;
    let link = link_state(&place);
    let Some(marker) = read_marker(&place)? else {
        return match link {
            Link::Factory => Err(Failure::precondition(format!(
                "{} runs factory lab but {} is gone, so there is no link to put back; link {} to today's devlab by hand",
                place.link.display(),
                place.marker().display(),
                place.link.display()
            ))),
            _ => Ok(Done::new(
                format!(
                    "not cut over: {} is today's devlab; nothing to roll back",
                    place.link.display()
                ),
                json!({"devlab": place.link, "rolledBack": false}),
            )),
        };
    };
    if !force {
        lab_holds_nothing()?;
    }
    let unit_gone = fs::symlink_metadata(&place.watcher).is_err();
    let link_unit = match &marker.watcher {
        Unit::Link(text) if unit_gone => Some(text),
        Unit::Unrecorded | Unit::File if unit_gone => {
            return Err(Failure::precondition(format!(
                "{} is gone and {} names no link to put back; link it again (dots link), then run factory cutover --rollback again",
                place.watcher.display(),
                place.marker().display()
            )));
        }
        _ => None,
    };
    if !matches!(&link, Link::Old(text) if *text == marker.devlab) {
        relink(&place, &marker.devlab)?;
        if !force {
            // A call that found factory under the name just before it went may have leased.
            if let Err(failure) = lab_holds_nothing() {
                relink(&place, Path::new("factory"))?;
                return Err(failure);
            }
        }
    }
    if let Some(text) = link_unit {
        link_watcher(&place, text, "cutover --rollback")?;
    }
    let retire = fs::symlink_metadata(&place.watcher).is_ok();
    if retire {
        Systemctl
            .enable_now(WATCHER)
            .map_err(systemd_failed("cutover --rollback"))?;
    }
    remove(&place.marker(), "cutover --rollback")?;
    Ok(Done::new(
        if retire {
            format!(
                "rolled back: {} runs today's devlab again and {WATCHER} is enabled; cut over again with factory cutover",
                place.link.display()
            )
        } else {
            format!(
                "rolled back: {} runs today's devlab again; cut over again with factory cutover",
                place.link.display()
            )
        },
        json!({
            "devlab": place.link,
            "restored": marker.devlab,
            "watcher": if retire { "enabled" } else { "not installed" },
        }),
    ))
}

/// Disables and stops the watcher, keeping its unit file. systemd's `disable` deletes a unit
/// file that is a link (as stow links it) along with its `[Install]` links, so the link is put
/// back at once: the watcher stays off at login, and `enable --now` can start it again.
fn retire_watcher(place: &Place, systemd: &impl UserManager, unit: &Unit) -> Result<(), Failure> {
    systemd
        .disable(WATCHER)
        .map_err(systemd_failed("cutover"))?;
    if let Unit::Link(text) = unit {
        link_watcher(place, text, "cutover")?;
        // disable reloaded systemd while the link was gone: let it see the unit again.
        systemd.reload().map_err(systemd_failed("cutover"))?;
    }
    systemd.stop(&[WATCHER]).map_err(systemd_failed("cutover"))
}

/// Links the watcher's unit file to `text` again, unless it is there.
fn link_watcher(place: &Place, text: &Path, command: &str) -> Result<(), Failure> {
    if fs::symlink_metadata(&place.watcher).is_ok() {
        return Ok(());
    }
    link_at(&place.watcher, text).map_err(|e| {
        Failure::precondition(format!(
            "cannot link {} to {} again: {e}; link it (dots link), then run factory {command} again",
            place.watcher.display(),
            text.display()
        ))
    })
}

/// What today's devlab's `status --json` shows held.
fn status_shows(devlab: &impl OldDevlab, program: &Path) -> Result<Vec<String>, Failure> {
    let again = "then run factory cutover again";
    let answer = devlab.call(&["status", "--json"]).map_err(|e| {
        Failure::precondition(format!("today's devlab did not answer: {e}; {again}"))
    })?;
    if answer.code != Some(0) {
        let said = answer
            .stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("it said nothing");
        return Err(Failure::precondition(format!(
            "{} status --json failed ({said}); fix it, {again}",
            program.display()
        )));
    }
    shown(&answer.stdout)
        .map_err(|e| Failure::precondition(format!("{e} from {}; {again}", program.display())))
}

/// Whether today's devlab refuses now that the marker is written.
fn refuses(devlab: &impl OldDevlab) -> bool {
    devlab
        .call(&["status"])
        .is_ok_and(|answer| answer.code != Some(0) && answer.stderr.contains(REFUSED))
}

/// Writes the marker while holding today's devlab's write lock, unless its ledger holds a lease
/// or a waiting request: a call under way either committed before (and is seen here) or meets
/// the marker at its next transaction.
fn mark_unless_held(place: &Place, old: &Path, unit: &Unit) -> Result<(), Failure> {
    let marker = Marker {
        devlab: old.to_owned(),
        factory: place.factory.clone(),
        at: clock::now().to_string(),
        watcher: unit.clone(),
    };
    let ledger = place.ledger();
    if !ledger.exists() {
        return write_marker(place, &marker);
    }
    let cannot = |e: rusqlite::Error| {
        Failure::precondition(format!(
            "cannot read {}: {e}; check it, then run factory cutover again",
            ledger.display()
        ))
    };
    let mut db =
        Connection::open_with_flags(&ledger, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(cannot)?;
    db.busy_timeout(BUSY_TIMEOUT).map_err(cannot)?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(cannot)?;
    write_marker(place, &marker)?;
    let undo = Undo { place };
    let held = match ledger_holds(&tx) {
        Ok(held) => held,
        Err(e) => return Err(undo.marker(cannot(e))),
    };
    if !held.is_empty() {
        let refusal = format!(
            "devlab.db holds {}, which devlab status did not show; {UNTIL_CUT}",
            named(&held)
        );
        return Err(undo.marker(Failure::new(Exit::Busy, refusal)));
    }
    tx.commit().map_err(|e| undo.marker(cannot(e)))
}

/// The leases and waiting requests in today's devlab's ledger; none from a table it lacks.
fn ledger_holds(db: &Connection) -> Result<Vec<String>, rusqlite::Error> {
    let has = |table: &str| -> Result<bool, rusqlite::Error> {
        db.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
    };
    let mut held = Vec::new();
    if has("leases")? {
        let mut rows = db.prepare("SELECT id, worktree, owner_label FROM leases ORDER BY id")?;
        let leases = rows.query_map([], |row| {
            Ok(lease(
                row.get(0)?,
                &row.get::<_, String>(1)?,
                &row.get::<_, String>(2)?,
            ))
        })?;
        for row in leases {
            held.push(row?);
        }
    }
    if has("waiters")? {
        let mut rows = db.prepare("SELECT label FROM waiters ORDER BY id")?;
        let waiters = rows.query_map([], |row| row.get::<_, String>(0))?;
        for row in waiters {
            held.push(format!("waiting {}", row?));
        }
    }
    Ok(held)
}

/// Refuses while factory lab holds a lease, a waiting request or a reservation.
fn lab_holds_nothing() -> Result<(), Failure> {
    let board = lab::board().map_err(|failure| {
        Failure::precondition(format!(
            "factory lab does not answer ({}), so what it holds is unknown; once nothing uses its devices, run factory cutover --rollback --force",
            failure.message
        ))
    })?;
    let held: Vec<String> = board
        .leases
        .iter()
        .map(|l| lease(l.id, &l.worktree, &l.owner_label))
        .chain(board.waiters.iter().map(|w| format!("waiting {}", w.label)))
        .chain(
            board
                .reservations
                .iter()
                .map(|r| format!("reservation {} ({})", r.id, r.owner_label)),
        )
        .collect();
    if held.is_empty() {
        return Ok(());
    }
    Err(Failure::new(
        Exit::Busy,
        busy("factory lab holds", &held, UNTIL_ROLLED),
    ))
}

/// Puts back what a cutover that failed part way had changed.
struct Undo<'a> {
    place: &'a Place,
}

impl Undo<'_> {
    /// Removes the marker, so today's devlab answers again; says so when it can't.
    fn marker(&self, failure: Failure) -> Failure {
        match fs::remove_file(self.place.marker()) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Failure::new(
                failure.exit,
                format!(
                    "{}; and {} could not be removed ({e}): remove it, or today's devlab keeps refusing",
                    failure.message,
                    self.place.marker().display()
                ),
            ),
            _ => failure,
        }
    }

    /// Starts the watcher again and removes the marker.
    fn watcher(&self, systemd: &impl UserManager, failure: Failure) -> Failure {
        let failure = match systemd.enable_now(WATCHER) {
            Ok(()) => failure,
            Err(e) => Failure::new(
                failure.exit,
                format!(
                    "{}; and {WATCHER} did not start again ({e})",
                    failure.message
                ),
            ),
        };
        self.marker(failure)
    }
}

fn read_marker(place: &Place) -> Result<Option<Marker>, Failure> {
    let path = place.marker();
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Failure::precondition(format!(
                "cannot read {}: {e}; check it, then run factory cutover --rollback again",
                path.display()
            )));
        }
    };
    serde_json::from_str(&text).map(Some).map_err(|e| {
        Failure::precondition(format!(
            "{} names no link to put back ({e}); link devlab to today's devlab by hand and remove it",
            path.display()
        ))
    })
}

/// Writes the marker whole, through a rename: today's devlab refuses once it exists.
fn write_marker(place: &Place, marker: &Marker) -> Result<(), Failure> {
    let path = place.marker();
    let cannot = |e: io::Error| {
        Failure::precondition(format!(
            "cannot write {}: {e}; check that {} is writable, then run factory cutover again",
            path.display(),
            place.state.display()
        ))
    };
    fs::create_dir_all(&place.state).map_err(cannot)?;
    let partial = place
        .state
        .join(format!(".cutover.{}.partial", std::process::id()));
    let text = serde_json::to_string(marker).unwrap_or_default() + "\n";
    fs::write(&partial, text)
        .and_then(|()| fs::rename(&partial, &path))
        .map_err(|e| {
            let _ = fs::remove_file(&partial);
            cannot(e)
        })
}

/// Points `~/.local/bin/devlab` at `target` through a rename, so the name is never missing.
fn relink(place: &Place, target: &Path) -> Result<(), Failure> {
    link_at(&place.link, target).map_err(|e| {
        Failure::precondition(format!(
            "cannot link {} to {}: {e}; check that {} is writable",
            place.link.display(),
            target.display(),
            place.bin.display()
        ))
    })
}

/// Makes `link` a link to `target` through a rename in its dir, so it is never half made.
fn link_at(link: &Path, target: &Path) -> io::Result<()> {
    let name = link.file_name().unwrap_or_default().to_string_lossy();
    let partial = link.with_file_name(format!(".{name}.{}.partial", std::process::id()));
    std::os::unix::fs::symlink(target, &partial)
        .and_then(|()| fs::rename(&partial, link))
        .inspect_err(|_| {
            let _ = fs::remove_file(&partial);
        })
}

fn remove(path: &Path, command: &str) -> Result<(), Failure> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(Failure::precondition(format!(
            "cannot remove {}: {e}; remove it, then run factory {command} again",
            path.display()
        ))),
        _ => Ok(()),
    }
}

/// A systemd failure, as one line that says to fix it and run `command` again.
fn systemd_failed(command: &str) -> impl Fn(SystemdError) -> Failure + '_ {
    move |e| Failure::precondition(format!("{e}; fix it and run factory {command} again"))
}
