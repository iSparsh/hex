//! One capture's AT-SPI observer. No clipboard reads, keys or tree polling.
//! Snapshot before preedit; confirm the exact selection replacement after
//! commit; subsequently read only this field on text-change events.
use super::*;
use crate::dictation::correction_learning::Insertion;
use futures_util::StreamExt;
use std::sync::{
    LazyLock, Mutex,
    atomic::{AtomicU64, Ordering},
    mpsc::{self, Receiver, SyncSender},
};
use std::time::Instant;

static GENERATION: AtomicU64 = AtomicU64::new(0);
static PENDING: LazyLock<Mutex<Option<Session>>> = LazyLock::new(|| Mutex::new(None));
const TEXT: &str = "org.a11y.atspi.Text";
const MAX_CHARS: i32 = 32_768;
const FOCUSED: u32 = 12;
const EDITABLE: u32 = 7;

pub(crate) struct Session {
    receipt: SyncSender<(String, String)>,
}

impl Session {
    /// A receipt is sent only for a successfully inserted, unsent dictation.
    pub(crate) fn inserted(self, text: &str, application: &str) {
        let _ = self
            .receipt
            .try_send((text.to_owned(), application.to_owned()));
    }
}

/// Starts on the capture owner before provisional words can appear. Slow
/// accessibility work stays on a worker. A new capture invalidates old work.
pub(crate) fn arm() {
    cancel();
    if !crate::labs::correction_learning() {
        return;
    }
    let generation = GENERATION.load(Ordering::Acquire);
    let (sender, receiver) = mpsc::sync_channel(1);
    if WORKER.as_ref().is_some_and(|worker| {
        worker
            .try_send(Observation {
                generation,
                receipt: receiver,
            })
            .is_ok()
    }) {
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(Session { receipt: sender });
    }
}

struct Observation {
    generation: u64,
    receipt: Receiver<(String, String)>,
}

// One worker and one queued capture. A normal new capture cancels its old
// observation and starts as soon as that observation yields (at most one
// event wait). Even a stalled compositor cannot accumulate observer threads.
static WORKER: LazyLock<Option<SyncSender<Observation>>> = LazyLock::new(|| {
    let (sender, receiver) = mpsc::sync_channel::<Observation>(1);
    std::thread::Builder::new()
        .name("hex-learn-edits".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            for observation in receiver {
                if !current(observation.generation) {
                    continue;
                }
                runtime.block_on(async {
                    if observe(observation.generation, observation.receipt)
                        .await
                        .is_none()
                    {
                        tracing::debug!("correction observation ended or target was unavailable");
                    }
                });
            }
        })
        .ok()
        .map(|_| sender)
});

pub(crate) fn take() -> Option<Session> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).take()
}

pub(crate) fn cancel() {
    GENERATION.fetch_add(1, Ordering::AcqRel);
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
}

fn allowed(application: &str) -> bool {
    crate::app_settings::AppSettings::saved_labs()
        .correction_learning_apps
        .iter()
        .any(|a| a.eq_ignore_ascii_case(application))
}

fn current(generation: u64) -> bool {
    GENERATION.load(Ordering::Acquire) == generation && crate::labs::correction_learning()
}

struct Target {
    field: Reference,
    window: Reference,
    before: String,
    selection: (usize, usize),
    application: String,
}

async fn connect() -> Option<Connection> {
    let session = Connection::session().await.ok()?;
    let address: String = call(
        &session,
        "org.a11y.Bus",
        "/org/a11y/bus",
        "org.a11y.Bus",
        "GetAddress",
        &(),
    )
    .await?;
    zbus::connection::Builder::address(address.as_str())
        .build()
        .await
        .ok()
}

async fn target(bus: &Connection) -> Option<Target> {
    tracing::debug!("correction observer connected; locating field");
    let context = crate::linux::context::capture(false);
    tracing::debug!(application=?context.application, "correction context");
    let application = context.application?;
    tracing::debug!(%application, "correction target application");
    if !allowed(&application) {
        return None;
    }
    let hint = crate::linux::context::focused_hint()?;
    let apps: Vec<Reference> = call(bus, REGISTRY, ROOT, ACCESSIBLE, "GetChildren", &()).await?;
    let mut candidates = Vec::new();
    for (name, path) in apps {
        if let Some(window) = matching_window(bus, &name, &path, &hint).await {
            candidates.push(window);
        }
    }
    tracing::debug!(count = candidates.len(), "correction matching windows");
    if candidates.len() != 1 {
        return None;
    }
    let window = candidates.pop()?.reference;
    let mut states = bitset::<2>(FOCUSED);
    states[0] |= (1 << EDITABLE) | (1 << STATE_SHOWING);
    let rule: MatchRule = (
        states,
        MATCH_ALL,
        HashMap::new(),
        MATCH_ALL,
        vec![0; 4],
        MATCH_ANY,
        vec![TEXT.into()],
        MATCH_ALL,
        false,
    );
    let fields: Option<Vec<Reference>> = call(
        bus,
        &window.0,
        window.1.as_str(),
        "org.a11y.atspi.Collection",
        "GetMatches",
        &(rule, SORT_CANONICAL, 2_i32, true),
    )
    .await;
    let fields = match fields.filter(|fields| !fields.is_empty()) {
        Some(fields) => fields,
        None => focused_fields(bus, &window).await?,
    };
    tracing::debug!(count = fields.len(), "correction focused editable fields");
    if fields.len() != 1 {
        return None;
    }
    let field = fields.into_iter().next()?;
    if !safe(bus, &field, &window).await? {
        return None;
    }
    let before = text(bus, &field).await?;
    let count: i32 = call(bus, &field.0, field.1.as_str(), TEXT, "GetNSelections", &()).await?;
    let selection = match count {
        0 => {
            let caret = property_i32(bus, &field, TEXT, "CaretOffset").await?;
            (caret, caret)
        }
        1 => {
            call(
                bus,
                &field.0,
                field.1.as_str(),
                TEXT,
                "GetSelection",
                &(0_i32,),
            )
            .await?
        }
        _ => return None,
    };
    let offset = |value: i32| -> Option<usize> {
        let index = usize::try_from(value).ok()?;
        if index == before.chars().count() {
            Some(before.len())
        } else {
            before.char_indices().nth(index).map(|(i, _)| i)
        }
    };
    let selection = (
        offset(selection.0.min(selection.1))?,
        offset(selection.0.max(selection.1))?,
    );
    tracing::debug!(
        bytes = before.len(),
        ?selection,
        "correction initial snapshot"
    );
    Some(Target {
        field,
        window,
        before,
        selection,
        application,
    })
}

/// GTK does not expose Collection on every window. Fall back to a single
/// bounded initial walk; later observation never searches the tree again.
async fn focused_fields(bus: &Connection, window: &Reference) -> Option<Vec<Reference>> {
    let mut pending = vec![(window.clone(), 0)];
    let mut found = Vec::new();
    let mut visited = 0;
    while let Some((node, depth)) = pending.pop() {
        visited += 1;
        if visited > 256 || depth > 32 {
            return None;
        }
        let role: u32 = call(bus, &node.0, node.1.as_str(), ACCESSIBLE, "GetRole", &()).await?;
        if role == 40 || role == 60 {
            continue;
        }
        let states: Vec<u32> =
            call(bus, &node.0, node.1.as_str(), ACCESSIBLE, "GetState", &()).await?;
        if !has_state(&states, STATE_SHOWING) {
            continue;
        }
        if has_state(&states, FOCUSED) && has_state(&states, EDITABLE) {
            let interfaces: Vec<String> = call(
                bus,
                &node.0,
                node.1.as_str(),
                ACCESSIBLE,
                "GetInterfaces",
                &(),
            )
            .await?;
            if interfaces.iter().any(|i| i == TEXT) {
                found.push(node.clone());
            }
            if found.len() > 1 {
                return None;
            }
        }
        let children: Vec<Reference> = call(
            bus,
            &node.0,
            node.1.as_str(),
            ACCESSIBLE,
            "GetChildren",
            &(),
        )
        .await?;
        if children.len() + visited + pending.len() > 256 {
            return None;
        }
        pending.extend(
            children
                .into_iter()
                .filter(|c| c.1.as_str() != NULL)
                .map(|child| (child, depth + 1)),
        );
    }
    Some(found)
}

async fn property_i32(
    bus: &Connection,
    field: &Reference,
    interface: &str,
    name: &str,
) -> Option<i32> {
    let value: OwnedVariant = call(
        bus,
        &field.0,
        field.1.as_str(),
        "org.freedesktop.DBus.Properties",
        "Get",
        &(interface, name),
    )
    .await?;
    i32::try_from(value).ok()
}

async fn text(bus: &Connection, field: &Reference) -> Option<String> {
    let count = property_i32(bus, field, TEXT, "CharacterCount").await?;
    if !(0..=MAX_CHARS).contains(&count) {
        return None;
    }
    let value: String = call(
        bus,
        &field.0,
        field.1.as_str(),
        TEXT,
        "GetText",
        &(0_i32, count),
    )
    .await?;
    (value.chars().count() == count as usize).then_some(value)
}

async fn safe(bus: &Connection, field: &Reference, window: &Reference) -> Option<bool> {
    let states: Vec<u32> =
        call(bus, &field.0, field.1.as_str(), ACCESSIBLE, "GetState", &()).await?;
    if ![FOCUSED, EDITABLE, STATE_SHOWING]
        .iter()
        .all(|s| has_state(&states, *s))
    {
        return Some(false);
    }
    let states: Vec<u32> = call(
        bus,
        &window.0,
        window.1.as_str(),
        ACCESSIBLE,
        "GetState",
        &(),
    )
    .await?;
    if !has_state(&states, STATE_ACTIVE) {
        return Some(false);
    }
    let mut node = field.clone();
    for _ in 0..32 {
        let role: u32 = call(bus, &node.0, node.1.as_str(), ACCESSIBLE, "GetRole", &()).await?;
        if role == 40 || role == 60 {
            return Some(false);
        } // password text, terminal
        let attrs: HashMap<String, String> = call(
            bus,
            &node.0,
            node.1.as_str(),
            ACCESSIBLE,
            "GetAttributes",
            &(),
        )
        .await?;
        if attrs
            .get("text-input-type")
            .is_some_and(|s| s == "password")
        {
            return Some(false);
        }
        if node == *window {
            return Some(true);
        }
        let parent: OwnedVariant = call(
            bus,
            &node.0,
            node.1.as_str(),
            "org.freedesktop.DBus.Properties",
            "Get",
            &(ACCESSIBLE, "Parent"),
        )
        .await?;
        node = Reference::try_from(parent).ok()?;
    }
    Some(false)
}

async fn observe(generation: u64, receiver: Receiver<(String, String)>) -> Option<()> {
    let bus = tokio::time::timeout(Duration::from_secs(1), connect())
        .await
        .ok()??;
    let target = tokio::time::timeout(Duration::from_millis(750), target(&bus))
        .await
        .ok()??;
    if !current(generation) {
        return None;
    }
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(target.field.0.as_str())
        .interface("org.a11y.atspi.Event.Object")
        .build()
        .ok()?;
    let mut events =
        zbus::MessageStream::for_match_rule(rule, &bus, std::num::NonZeroUsize::new(128))
            .await
            .ok()?;
    for event in [
        "object:text-changed",
        "object:state-changed",
        "object:children-changed",
    ] {
        let _: () = call(
            &bus,
            REGISTRY,
            "/org/a11y/atspi/registry",
            REGISTRY,
            "RegisterEvent",
            &(event, Vec::<String>::new(), target.field.0.as_str()),
        )
        .await?;
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    let receipt = loop {
        if !current(generation) || Instant::now() >= deadline {
            return None;
        }
        match receiver.try_recv() {
            Ok(receipt) => break receipt,
            Err(mpsc::TryRecvError::Disconnected) => return None,
            Err(mpsc::TryRecvError::Empty) => {
                if let Ok(message) =
                    tokio::time::timeout(Duration::from_millis(25), events.next()).await
                {
                    let message = message?.ok()?;
                    if lost_focus(&message, &target.field)? {
                        return None;
                    }
                }
            }
        }
    };
    tracing::debug!(
        application_matches = (receipt.1 == target.application),
        "correction insertion receipt"
    );
    if receipt.1 != target.application || !safe(&bus, &target.field, &target.window).await? {
        return None;
    }
    // Paste delivery can precede the app's processing of its clipboard or IME
    // event. Only the exact expected selection replacement establishes baseline.
    let mut insertion = None;
    for _ in 0..10 {
        if !current(generation) {
            return None;
        }
        let field = text(&bus, &target.field).await?;
        insertion = Insertion::locate(&target.before, &field, &receipt.0, target.selection);
        tracing::debug!(
            matched = insertion.is_some(),
            bytes = field.len(),
            "correction insertion verification"
        );
        if insertion.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let insertion = insertion?;
    // Drop preedit/insertion events before enabling correction interpretation.
    while let Some(Some(message)) = events.next().now_or_never() {
        if lost_focus(&message.ok()?, &target.field)? {
            return None;
        }
    }
    tracing::debug!("correction observer verified final insertion");
    let mut dirty = false;
    let mut changed = Instant::now();
    let mut last_read = Instant::now();
    loop {
        if !current(generation) || Instant::now() >= deadline {
            return None;
        }
        if let Ok(message) = tokio::time::timeout(Duration::from_millis(100), events.next()).await {
            let message = message?.ok()?;
            if lost_focus(&message, &target.field)? {
                return None;
            }
            if message.header().path().map(|p| p.as_str()) == Some(target.field.1.as_str()) {
                match message.header().member().map(|m| m.as_str()) {
                    Some("TextChanged") => {
                        dirty = true;
                        changed = Instant::now();
                    }
                    Some("StateChanged") if !safe(&bus, &target.field, &target.window).await? => {
                        return None;
                    }
                    _ => {}
                }
            }
        }
        if last_read.elapsed() >= Duration::from_secs(1) {
            if !allowed(&target.application) || !safe(&bus, &target.field, &target.window).await? {
                return None;
            }
            last_read = Instant::now();
        }
        if dirty && changed.elapsed() >= Duration::from_millis(900) {
            if !safe(&bus, &target.field, &target.window).await? {
                return None;
            }
            let field = text(&bus, &target.field).await?;
            insertion.edited(&field)?;
            if let Some((heard, intended)) = insertion.correction(&field) {
                if !current(generation) || !allowed(&target.application) {
                    return None;
                }
                if let Some(id) = crate::dictation::correction_learning::record(
                    &target.application,
                    heard,
                    intended,
                )
                .ok()?
                    && crate::labs::correction_notices()
                {
                    notify(id);
                }
                return Some(());
            }
            dirty = false;
        }
    }
}

// Read the event's state transition, not just the current state. A field
// losing and regaining focus between reads must still invalidate the session.
fn lost_focus(message: &zbus::Message, field: &Reference) -> Option<bool> {
    let header = message.header();
    if header.path().map(|p| p.as_str()) != Some(field.1.as_str())
        || header.member().map(|m| m.as_str()) != Some("StateChanged")
    {
        return Some(false);
    }
    type Body = (
        String,
        i32,
        i32,
        OwnedVariant,
        HashMap<String, OwnedVariant>,
    );
    let (kind, enabled, _, _, _) = message.body().deserialize::<Body>().ok()?;
    Some(
        (kind == "focused" && enabled == 0)
            || (kind == "defunct" && enabled != 0)
            || (kind == "editable" && enabled == 0),
    )
}

static NOTIFICATION_GENERATION: AtomicU64 = AtomicU64::new(0);

fn notify(id: u64) {
    let generation = NOTIFICATION_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    let _ = std::thread::Builder::new()
        .name("hex-learned-notice".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            let _ = runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(15), notification(id, generation)).await
            });
        });
}

async fn notification(id: u64, generation: u64) {
    // Use the desktop protocol directly: no external notification executable
    // or shell. Correction contents stay out of the notification and logs.
    let Ok(bus) = Connection::session().await else {
        return;
    };
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.Notifications")
        .interface("org.freedesktop.Notifications")
        .member("ActionInvoked")
        .build();
    let Ok(rule) = rule else {
        return;
    };
    let Ok(mut actions) =
        zbus::MessageStream::for_match_rule(rule, &bus, std::num::NonZeroUsize::new(8)).await
    else {
        return;
    };
    let notification: Option<u32> = call(
        &bus,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        "Notify",
        &(
            "Hex",
            0_u32,
            "",
            "Learned a dictation correction",
            format!("Saved as a contextual AI hint. Review: hex corrections list (id {id})"),
            vec!["undo", "Undo"],
            HashMap::<String, OwnedVariant>::new(),
            10_000_i32,
        ),
    )
    .await;
    let Some(notification) = notification else {
        return;
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    while tokio::time::Instant::now() < deadline
        && NOTIFICATION_GENERATION.load(Ordering::Acquire) == generation
        && crate::labs::correction_notices()
    {
        let Ok(Some(Ok(message))) =
            tokio::time::timeout(Duration::from_millis(100), actions.next()).await
        else {
            continue;
        };
        if let Ok((clicked, action)) = message.body().deserialize::<(u32, String)>()
            && clicked == notification
            && action == "undo"
        {
            let _ = crate::dictation::correction_learning::forget(id);
            break;
        }
    }
}

use futures_util::FutureExt;

#[cfg(test)]
mod tests {
    use super::*;

    /// Opens its own GTK window; requires an unlocked Wayland/X11 desktop,
    /// Python GI + GTK4 and an explicitly isolated HEX_APPLICATION_SUPPORT_DIR.
    /// Exercises real AT-SPI events and persistence without audio or key input.
    #[test]
    #[ignore = "opens a disposable GTK window; requires isolated data and a desktop"]
    fn learns_a_real_field_edit_without_creating_an_alias() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .try_init();
        let root = std::path::PathBuf::from(
            std::env::var_os("HEX_APPLICATION_SUPPORT_DIR")
                .expect("set isolated HEX_APPLICATION_SUPPORT_DIR"),
        );
        assert!(
            !root.join("settings.json").exists(),
            "probe requires an empty data root"
        );
        std::fs::create_dir_all(&root).unwrap();
        let mut settings = crate::app_settings::AppSettings::default();
        settings.labs.correction_learning = true;
        let python = std::process::Command::new("python3")
            .args([
                "-c",
                "import os,sys; print(os.path.basename(os.path.realpath(sys.executable)))",
            ])
            .output()
            .unwrap();
        let application = String::from_utf8(python.stdout)
            .unwrap()
            .trim()
            .to_lowercase();
        settings.labs.correction_learning_apps = vec![application.clone()];
        let saved = serde_json::to_vec_pretty(&settings).unwrap();
        std::fs::write(root.join("settings.json"), &saved).unwrap();
        crate::labs::apply(&settings.labs);
        let mut child = std::process::Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/scripts/probes/correction-learning.py"
            ))
            .arg(&root)
            .env("GTK_A11Y", "atspi")
            .spawn()
            .unwrap();
        struct Cleanup<'a>(&'a std::path::Path, &'a mut std::process::Child);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = std::fs::write(self.0.join("close"), "");
                let _ = self.1.kill();
                let _ = self.1.wait();
                cancel();
            }
        }
        let _cleanup = Cleanup(&root, &mut child);
        let wait = |name: &str| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !root.join(name).exists() {
                assert!(Instant::now() < deadline, "timed out waiting for {name}");
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        wait("ready");
        arm();
        std::thread::sleep(Duration::from_millis(1200));
        std::fs::write(root.join("insert"), "").unwrap();
        wait("insert-done");
        take()
            .unwrap()
            .inserted("Ask cloud to review this diff.", &application);
        std::thread::sleep(Duration::from_millis(500));
        std::fs::write(root.join("correct"), "").unwrap();
        wait("correct-done");
        wait("corrections.json");
        let ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("corrections.json")).unwrap()).unwrap();
        assert_eq!(ledger["entries"][0]["heard"], "cloud");
        assert_eq!(ledger["entries"][0]["intended"], "Claude");
        assert_eq!(std::fs::read(root.join("settings.json")).unwrap(), saved);
        let hints =
            crate::dictation::correction_learning::hints(&application, "Ask cloud to review");
        assert!(hints.contains("Claude"));
        assert!(hints.contains("Never apply as unconditional"));
        assert!(crate::dictation::correction_learning::hints("Other app", "Ask cloud").is_empty());
    }
}
