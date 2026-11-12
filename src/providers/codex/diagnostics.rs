//! Opt-in observations only. No diagnostic value is an upstream control input.
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::config::PromptCacheKeyMode;
use crate::request_identity::ConversationIdentity;

const TTL: Duration = Duration::from_secs(30 * 60);
const CAPACITY: usize = 1024;
const MAX_BYTES: u64 = 1024 * 1024;
const QUEUE_CAPACITY: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Main,
    Agent,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Model {
    Sol,
    Terra,
    Luna,
    Other,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Lane {
    Full,
    Lite,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CacheMode {
    Session,
    Thread,
    Omit,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Bucket {
    Unknown,
    Under1s,
    S1To10,
    S10To60,
    M1To5,
    M5To30,
    M30Plus,
}
impl Bucket {
    fn from_duration(duration: Option<Duration>) -> Self {
        match duration.map(|value| value.as_secs()) {
            None => Self::Unknown,
            Some(0) => Self::Under1s,
            Some(1..=9) => Self::S1To10,
            Some(10..=59) => Self::S10To60,
            Some(60..=299) => Self::M1To5,
            Some(300..=1799) => Self::M5To30,
            Some(_) => Self::M30Plus,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CacheHit {
    Unknown,
    FullMiss,
    Partial,
    FullHit,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Outcome {
    Success,
    Failure,
    Cancelled,
    MissingTerminal,
    Excluded,
}

// Only allow-listed enums and validated upstream counts can reach the sink.
#[derive(Debug, Serialize)]
struct Record {
    schema: u8,
    kind: Kind,
    cache_key_mode: CacheMode,
    model: Model,
    lane: Lane,
    gap: Bucket,
    duration: Bucket,
    outcome: Outcome,
    input_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_hit: CacheHit,
}

struct Entry {
    epoch: u64,
    generation: u64,
    active: usize,
    ambiguous: bool,
    completed: Option<Instant>,
    touched: Instant,
}
#[derive(Default)]
struct State {
    entries: HashMap<ConversationIdentity, Entry>,
    sequence: u64,
    active: usize,
    admission_ambiguous: bool,
}
struct Reservation {
    owner: ConversationIdentity,
    epoch: u64,
    generation: u64,
}
struct Pending {
    counted: bool,
    reservation: Option<Reservation>,
    started: Instant,
    record: Record,
}

impl State {
    fn begin(&mut self, owner: Option<&ConversationIdentity>, now: Instant) -> Pending {
        self.entries.retain(|_, entry| {
            entry.active > 0
                || now
                    .checked_duration_since(entry.touched)
                    .is_some_and(|age| age < TTL)
        });
        let mut gap = Bucket::Unknown;
        let reservation = owner.and_then(|owner| {
            self.active = self.active.saturating_add(1);
            if !self.entries.contains_key(owner) && self.entries.len() >= CAPACITY {
                let oldest = self
                    .entries
                    .iter()
                    .filter(|(_, entry)| entry.active == 0)
                    .min_by_key(|(_, entry)| entry.touched)
                    .map(|(key, _)| key.clone());
                if let Some(oldest) = oldest {
                    self.entries.remove(&oldest);
                } else {
                    // Refused identities may overlap later admissions: no known gaps
                    // until all outstanding reservations (including refusals) drain.
                    self.admission_ambiguous = true;
                    for entry in self.entries.values_mut() {
                        entry.completed = None;
                    }
                    return None;
                }
            }
            self.sequence = self.sequence.wrapping_add(1);
            let sequence = self.sequence;
            let entry = self.entries.entry(owner.clone()).or_insert(Entry {
                epoch: sequence,
                generation: sequence,
                active: 0,
                ambiguous: false,
                completed: None,
                touched: now,
            });
            if entry.active == 0 {
                gap = Bucket::from_duration(
                    entry
                        .completed
                        .and_then(|previous| now.checked_duration_since(previous)),
                );
            } else {
                entry.ambiguous = true;
            }
            entry.completed = None;
            entry.active = entry.active.saturating_add(1);
            entry.generation = sequence;
            entry.touched = now;
            Some(Reservation {
                owner: owner.clone(),
                epoch: entry.epoch,
                generation: sequence,
            })
        });
        if self.admission_ambiguous {
            gap = Bucket::Unknown;
        }
        Pending {
            counted: owner.is_some(),
            reservation,
            started: now,
            record: Record {
                schema: 1,
                kind: match owner {
                    Some(ConversationIdentity::Main(_)) => Kind::Main,
                    Some(ConversationIdentity::Agent(_, _)) => Kind::Agent,
                    None => Kind::Unknown,
                },
                cache_key_mode: CacheMode::Unknown,
                model: Model::Other,
                lane: Lane::Unknown,
                gap,
                duration: Bucket::Unknown,
                outcome: Outcome::Cancelled,
                input_tokens: None,
                cache_read_tokens: None,
                cache_hit: CacheHit::Unknown,
            },
        }
    }

    fn finish(
        &mut self,
        mut pending: Pending,
        now: Instant,
        outcome: Outcome,
        usage: Option<&Value>,
    ) -> Record {
        let elapsed = now.checked_duration_since(pending.started);
        pending.record.duration = Bucket::from_duration(elapsed);
        pending.record.outcome = outcome;
        if let Some(reservation) = pending.reservation {
            match self.entries.get_mut(&reservation.owner) {
                Some(entry) if entry.epoch == reservation.epoch => {
                    let unambiguous = !self.admission_ambiguous
                        && !entry.ambiguous
                        && entry.generation == reservation.generation;
                    if !unambiguous {
                        pending.record.gap = Bucket::Unknown;
                    }
                    entry.active = entry.active.saturating_sub(1);
                    entry.completed =
                        (unambiguous && elapsed.is_some() && outcome == Outcome::Success)
                            .then_some(now);
                    entry.touched = now;
                    if entry.active == 0 {
                        entry.ambiguous = false;
                    }
                }
                _ => pending.record.gap = Bucket::Unknown,
            }
        }
        if pending.counted {
            self.active = self.active.saturating_sub(1);
            if self.active == 0 {
                self.admission_ambiguous = false;
            }
        }
        if outcome == Outcome::Success {
            let input = usage
                .and_then(|usage| usage.get("input_tokens"))
                .and_then(Value::as_u64);
            let cached = usage
                .and_then(|usage| usage.pointer("/input_tokens_details/cached_tokens"))
                .and_then(Value::as_u64);
            pending.record.input_tokens = input;
            pending.record.cache_read_tokens =
                cached.filter(|cached| input.is_some_and(|input| *cached <= input));
            pending.record.cache_hit = match (input, pending.record.cache_read_tokens) {
                (Some(input), Some(cached)) if input > 0 => {
                    if cached == 0 {
                        CacheHit::FullMiss
                    } else if cached == input {
                        CacheHit::FullHit
                    } else {
                        CacheHit::Partial
                    }
                }
                _ => CacheHit::Unknown,
            };
        }
        pending.record
    }
}

pub(super) fn enabled() -> bool {
    matches!(
        std::env::var("CCP_CODEX_THREAD_DIAGNOSTICS")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1" | "true" | "on")
    )
}

pub(super) struct Observer {
    state: Mutex<State>,
    sender: mpsc::SyncSender<Record>,
}
struct SharedObserver {
    directory: Option<std::path::PathBuf>,
    observer: Option<Arc<Observer>>,
}
impl SharedObserver {
    fn get(&mut self, directory: std::path::PathBuf) -> Option<Arc<Observer>> {
        if self.directory.as_ref() != Some(&directory) {
            self.observer = Observer::configured(Some("true"), Some(&directory));
            self.directory = Some(directory);
        }
        self.observer.clone()
    }
}
impl Observer {
    pub fn from_env() -> Option<Arc<Self>> {
        static OBSERVER: Mutex<SharedObserver> = Mutex::new(SharedObserver {
            directory: None,
            observer: None,
        });
        let enabled = std::env::var("CCP_CODEX_THREAD_DIAGNOSTICS").ok();
        if !matches!(enabled.as_deref().map(str::trim), Some("1" | "true" | "on")) {
            return None;
        }
        let directory =
            std::path::PathBuf::from(std::env::var_os("CCP_CODEX_THREAD_DIAGNOSTICS_DIR")?);
        OBSERVER.lock().ok()?.get(directory)
    }

    fn configured(enabled: Option<&str>, directory: Option<&Path>) -> Option<Arc<Self>> {
        if !matches!(enabled.map(str::trim), Some("1" | "true" | "on")) {
            return None;
        }
        let directory = directory?.to_owned();
        Self::start_writer(move || Sink::create(&directory))
    }

    fn start_writer(
        create: impl FnOnce() -> std::io::Result<Sink> + Send + 'static,
    ) -> Option<Arc<Self>> {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("codex-diagnostics".into())
            .spawn(move || {
                let Ok(mut sink) = create() else {
                    return;
                };
                while let Ok(record) = receiver.recv() {
                    if sink.write(&record).is_err() {
                        break;
                    }
                }
            })
            .ok()?;
        Some(Arc::new(Self {
            state: Mutex::new(State::default()),
            sender,
        }))
    }

    pub fn begin(self: &Arc<Self>, owner: Option<&ConversationIdentity>, now: Instant) -> Turn {
        let pending = self
            .state
            .lock()
            .ok()
            .map(|mut state| state.begin(owner, now));
        Turn(Arc::new(TurnInner {
            observer: self.clone(),
            pending: Mutex::new(pending),
        }))
    }

    fn finish(&self, pending: Pending, now: Instant, outcome: Outcome, usage: Option<&Value>) {
        let record = self
            .state
            .lock()
            .ok()
            .map(|mut state| state.finish(pending, now, outcome, usage));
        if let Some(record) = record {
            // Never wait for disk or a full queue on the inference path.
            let _ = self.sender.try_send(record);
        }
    }
}

#[derive(Default)]
pub(super) struct Receipt {
    translator: Option<super::translate::live_stream::LiveStreamTranslator>,
    completed: Option<(Instant, Outcome, Option<u64>, Option<u64>)>,
    turn: Option<std::sync::Weak<TurnInner>>,
}
impl Receipt {
    pub fn observe(&mut self, payload: &Value) {
        // Reuse acceptance semantics, including signature-only reasoning, without traffic capture.
        let translator = self.translator.get_or_insert_with(|| {
            super::translate::live_stream::LiveStreamTranslator::new("diagnostic", "diagnostic")
        });
        let _ = translator.accept(payload, None);
        let semantic = translator.has_semantic_output();
        if !semantic && super::events::event_is_terminal(payload) {
            self.translator = None;
        }
        if semantic && super::events::event_is_terminal(payload) {
            let outcome = if super::events::event_is_success_terminal(payload) {
                Outcome::Success
            } else {
                Outcome::Failure
            };
            self.completed = Some((
                Instant::now(),
                outcome,
                payload
                    .pointer("/response/usage/input_tokens")
                    .and_then(Value::as_u64),
                payload
                    .pointer("/response/usage/input_tokens_details/cached_tokens")
                    .and_then(Value::as_u64),
            ));
            self.deliver();
        }
    }
    pub fn attach(&mut self, turn: &Turn) {
        self.turn = Some(Arc::downgrade(&turn.0));
        self.deliver();
    }
    fn deliver(&self) {
        if let Some((now, outcome, input, cached)) = self.completed
            && let Some(turn) = self.turn.as_ref().and_then(std::sync::Weak::upgrade)
        {
            Turn(turn).finish(
                now,
                outcome,
                Some(&serde_json::json!({
                    "input_tokens": input, "input_tokens_details": {"cached_tokens": cached}
                })),
            );
        }
    }
}

#[derive(Clone)]
pub(super) struct Turn(Arc<TurnInner>);
struct TurnInner {
    observer: Arc<Observer>,
    pending: Mutex<Option<Pending>>,
}
impl Turn {
    pub fn configure(&self, mode: PromptCacheKeyMode, model: &str, lite: bool) {
        if let Ok(mut pending) = self.0.pending.lock()
            && let Some(pending) = pending.as_mut()
        {
            pending.record.cache_key_mode = match mode {
                PromptCacheKeyMode::Session => CacheMode::Session,
                PromptCacheKeyMode::Thread => CacheMode::Thread,
                PromptCacheKeyMode::Omit => CacheMode::Omit,
            };
            pending.record.model = match model {
                "gpt-5.6-sol" | "gpt-6-sol" | "gpt-6.1-sol" => Model::Sol,
                "gpt-5.6-terra" | "gpt-6-terra" => Model::Terra,
                "gpt-5.6-luna" | "gpt-6-luna" => Model::Luna,
                _ => Model::Other,
            };
            pending.record.lane = if lite { Lane::Lite } else { Lane::Full };
        }
    }
    pub fn error(&self, error: &super::client::CodexError) {
        let missing_terminal = error.status == 0
            && matches!(
                (error.origin, error.detail.as_deref()),
                (
                    super::client::CodexErrorOrigin::Http
                        | super::client::CodexErrorOrigin::BufferedHttp,
                    Some("http_response_sse" | "http_response_body")
                ) | (
                    super::client::CodexErrorOrigin::WebSocket
                        | super::client::CodexErrorOrigin::BufferedWebSocket,
                    Some(super::websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL)
                )
            );
        self.end(if missing_terminal {
            Outcome::MissingTerminal
        } else {
            Outcome::Failure
        });
    }
    pub fn end(&self, outcome: Outcome) {
        self.finish(Instant::now(), outcome, None);
    }
    fn finish(&self, now: Instant, outcome: Outcome, usage: Option<&Value>) {
        let pending = self
            .0
            .pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.take());
        if let Some(pending) = pending {
            self.0.observer.finish(pending, now, outcome, usage);
        }
    }
    pub fn terminal(&self, payload: &Value) {
        if super::events::event_is_success_terminal(payload) {
            self.finish(
                Instant::now(),
                Outcome::Success,
                payload.pointer("/response/usage"),
            );
        } else {
            self.end(Outcome::Failure);
        }
    }
    pub fn buffered_error(&self, body: &[u8]) {
        let missing_terminal = super::translate::reducer::reduce_upstream_bytes(body)
            .err()
            .and_then(|error| error.diagnostics)
            .is_some_and(|diagnostics| !diagnostics.saw_terminal_event);
        self.end(if missing_terminal {
            Outcome::MissingTerminal
        } else {
            Outcome::Failure
        });
    }
    pub fn buffered(&self, body: &[u8]) {
        for event in crate::anthropic::sse::parse_sse_events(body) {
            if let Ok(payload) = serde_json::from_str::<Value>(&event.data)
                && super::events::event_is_terminal(&payload)
            {
                self.terminal(&payload);
                return;
            }
        }
        self.end(Outcome::MissingTerminal);
    }
}
impl Drop for TurnInner {
    fn drop(&mut self) {
        if let Ok(pending) = self.pending.get_mut()
            && let Some(pending) = pending.take()
        {
            self.observer
                .finish(pending, Instant::now(), Outcome::Cancelled, None);
        }
    }
}

#[cfg(unix)]
struct Sink {
    directory: std::fs::File,
    file: std::fs::File,
    bytes: u64,
    backup: Option<rustix::fs::Stat>,
}
#[cfg(unix)]
impl Sink {
    fn create(directory: &Path) -> std::io::Result<Self> {
        use rustix::fs::{Mode, OFlags, mkdirat, open, openat};
        use std::path::Component;
        let absolute = std::path::absolute(directory)?;
        if absolute.file_name().is_none() {
            return Err(std::io::Error::other("diagnostic directory must be new"));
        }
        let mut components = absolute.components().peekable();
        let mut parent = open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        while let Some(component) = components.next() {
            match component {
                Component::RootDir | Component::CurDir => continue,
                Component::Normal(name) => {
                    if components.peek().is_none() {
                        mkdirat(&parent, name, Mode::from_raw_mode(0o700))?;
                    }
                    parent = openat(
                        &parent,
                        name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )?;
                }
                _ => return Err(std::io::Error::other("invalid diagnostic directory")),
            }
        }
        let directory = std::fs::File::from(parent);
        use std::os::unix::fs::MetadataExt;
        if directory.metadata()?.mode() & 0o077 != 0 {
            return Err(std::io::Error::other("diagnostic directory exposed"));
        }
        let file = std::fs::File::from(openat(
            &directory,
            "turns.jsonl",
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        Ok(Self {
            directory,
            file,
            bytes: 0,
            backup: None,
        })
    }
    fn write(&mut self, record: &Record) -> std::io::Result<()> {
        use rustix::fs::{AtFlags, Mode, OFlags, linkat, openat, statat, unlinkat};
        use std::io::Write;
        use std::os::unix::fs::MetadataExt;
        if self.directory.metadata()?.mode() & 0o077 != 0 {
            return Err(std::io::Error::other("diagnostic directory exposed"));
        }
        let actual = statat(&self.directory, "turns.jsonl", AtFlags::SYMLINK_NOFOLLOW)?;
        let expected = rustix::fs::fstat(&self.file)?;
        if actual.st_dev != expected.st_dev
            || actual.st_ino != expected.st_ino
            || actual.st_nlink != 1
            || self.file.metadata()?.len() != self.bytes
            || actual.st_mode & 0o077 != 0
        {
            return Err(std::io::Error::other("diagnostic output replaced"));
        }
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        if line.len() as u64 > MAX_BYTES {
            return Err(std::io::Error::other("diagnostic record too large"));
        }
        if self.bytes + line.len() as u64 > MAX_BYTES {
            let backup = "turns.previous.jsonl";
            if let Some(expected) = &self.backup {
                let actual = statat(&self.directory, backup, AtFlags::SYMLINK_NOFOLLOW)?;
                if actual.st_dev != expected.st_dev
                    || actual.st_ino != expected.st_ino
                    || actual.st_nlink != 1
                {
                    return Err(std::io::Error::other("diagnostic backup replaced"));
                }
                unlinkat(&self.directory, backup, AtFlags::empty())?;
            }
            // All mutations are relative to the private directory handle, never
            // a pathname that a writable ancestor can redirect during rotation.
            linkat(
                &self.directory,
                "turns.jsonl",
                &self.directory,
                backup,
                AtFlags::empty(),
            )?;
            self.backup = Some(rustix::fs::fstat(&self.file)?);
            unlinkat(&self.directory, "turns.jsonl", AtFlags::empty())?;
            self.file = std::fs::File::from(openat(
                &self.directory,
                "turns.jsonl",
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )?);
            self.bytes = 0;
        }
        self.file.write_all(&line)?;
        self.bytes += line.len() as u64;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn observer() -> (Arc<Observer>, mpsc::Receiver<Record>) {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        (
            Arc::new(Observer {
                state: Mutex::new(State::default()),
                sender,
            }),
            receiver,
        )
    }
    fn owner(label: &str) -> ConversationIdentity {
        ConversationIdentity::Main(label.into())
    }
    fn at(base: Instant, seconds: u64) -> Instant {
        base + Duration::from_secs(seconds)
    }
    fn usage(cached: u64) -> Value {
        json!({"input_tokens":100,"input_tokens_details":{"cached_tokens":cached}})
    }

    #[test]
    fn synthetic_replay_is_thread_local_and_contains_no_canaries() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/thread-cache-diagnostics.json"
        ))
        .unwrap();
        let base = Instant::now();
        let mut state = State::default();
        let mut output = String::new();
        for row in fixture["turns"].as_array().unwrap() {
            let mut headers = http::HeaderMap::new();
            for (name, value) in row["headers"].as_object().unwrap() {
                headers.insert(
                    http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    http::HeaderValue::from_str(value.as_str().unwrap()).unwrap(),
                );
            }
            let identity = ConversationIdentity::from_headers(&headers);
            let pending = state.begin(identity.as_ref(), at(base, row["start"].as_u64().unwrap()));
            let record = state.finish(
                pending,
                at(base, row["end"].as_u64().unwrap()),
                Outcome::Success,
                Some(&usage(row["cached"].as_u64().unwrap())),
            );
            let value = serde_json::to_value(&record).unwrap();
            assert_eq!(value["gap"], row["gap"]);
            assert_eq!(
                record.kind,
                match identity {
                    Some(ConversationIdentity::Main(_)) => Kind::Main,
                    Some(ConversationIdentity::Agent(_, _)) => Kind::Agent,
                    None => Kind::Unknown,
                }
            );
            output.push_str(&serde_json::to_string(&record).unwrap());
        }
        for canary in fixture["canaries"].as_array().unwrap() {
            assert!(!output.contains(canary.as_str().unwrap()));
        }
        assert_eq!(state.entries.len(), 3);
    }

    #[test]
    fn overlap_generation_ttl_and_capacity_are_bounded() {
        let mut state = State::default();
        let base = Instant::now();
        let id = owner("overlap");
        let first = state.begin(Some(&id), base);
        state.finish(first, at(base, 1), Outcome::Success, None);
        let a = state.begin(Some(&id), at(base, 2));
        let b = state.begin(Some(&id), at(base, 3));
        assert_eq!(
            state.finish(b, at(base, 4), Outcome::Success, None).gap,
            Bucket::Unknown
        );
        assert_eq!(
            state.finish(a, at(base, 5), Outcome::Success, None).gap,
            Bucket::Unknown
        );
        let following = state.begin(Some(&id), at(base, 6));
        assert_eq!(following.record.gap, Bucket::Unknown);
        state.finish(following, at(base, 7), Outcome::Success, None);
        let following = state.begin(Some(&id), at(base, 9));
        assert_eq!(following.record.gap, Bucket::S1To10);
        state.finish(following, at(base, 10), Outcome::Success, None);
        let expired = state.begin(Some(&id), at(base, 1810));
        assert_eq!(expired.record.gap, Bucket::Unknown);
        for index in 0..CAPACITY {
            let turn = state.begin(
                Some(&owner(&format!("capacity-{index}"))),
                at(base, 1811 + index as u64),
            );
            state.finish(turn, at(base, 1811 + index as u64), Outcome::Success, None);
        }
        assert_eq!(state.entries.len(), CAPACITY);
        let replacement = state.begin(Some(&id), at(base, 2900));
        assert_eq!(
            state
                .finish(replacement, at(base, 2900), Outcome::Success, None)
                .gap,
            Bucket::Unknown
        );
        let following = state.begin(Some(&id), at(base, 2901));
        assert_eq!(following.record.gap, Bucket::Unknown);
        state.finish(expired, at(base, 2901), Outcome::Success, None);
        assert_eq!(
            state
                .finish(following, at(base, 2902), Outcome::Success, None)
                .gap,
            Bucket::Unknown
        );
        assert_eq!(state.entries.len(), CAPACITY);

        let mut state = State::default();
        let mut active = Vec::new();
        for index in 0..CAPACITY {
            active.push(state.begin(Some(&owner(&format!("pinned-{index}"))), base));
        }
        let refused_id = owner("refused");
        let refused = state.begin(Some(&refused_id), at(base, 1801));
        assert!(refused.reservation.is_none());
        state.finish(
            active.pop().unwrap(),
            at(base, 1802),
            Outcome::Success,
            None,
        );
        let admitted = state.begin(Some(&refused_id), at(base, 1803));
        state.finish(admitted, at(base, 1804), Outcome::Success, None);
        let following = state.begin(Some(&refused_id), at(base, 1805));
        assert_eq!(following.record.gap, Bucket::Unknown);
        state.finish(following, at(base, 1806), Outcome::Success, None);
        state.finish(refused, at(base, 1807), Outcome::Success, None);
        for pending in active {
            state.finish(pending, at(base, 1808), Outcome::Success, None);
        }
        assert_eq!(state.active, 0);
        assert!(!state.admission_ambiguous);
        assert!(state.entries.len() <= CAPACITY);

        let (observer, receiver) = observer();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let barrier = barrier.clone();
                let observer = observer.clone();
                let id = id.clone();
                scope.spawn(move || {
                    let turn = observer.begin(Some(&id), Instant::now());
                    barrier.wait();
                    barrier.wait();
                    turn.terminal(
                        &json!({"type":"response.completed","response":{"usage":usage(0)}}),
                    );
                });
            }
            barrier.wait();
            barrier.wait();
        });
        for _ in 0..2 {
            assert_eq!(receiver.recv().unwrap().gap, Bucket::Unknown);
        }
        assert!(
            observer.state.lock().unwrap().entries[&id]
                .completed
                .is_none()
        );
    }

    #[test]
    fn lifecycle_usage_reversal_and_repeated_terminal_are_conservative() {
        let (observer, receiver) = observer();
        let id = owner("synthetic-session-canary");
        for (event, expected) in [
            (
                json!({"type":"response.completed","response":{"usage":usage(0)}}),
                CacheHit::FullMiss,
            ),
            (
                json!({"type":"response.done","response":{"usage":usage(100)}}),
                CacheHit::FullHit,
            ),
            (
                json!({"type":"response.completed","response":{"usage":usage(50)}}),
                CacheHit::Partial,
            ),
            (
                json!({"type":"response.completed","response":{"usage":usage(101)}}),
                CacheHit::Unknown,
            ),
            (
                json!({"type":"response.completed","response":{"usage":{"input_tokens":100}}}),
                CacheHit::Unknown,
            ),
            (
                json!({"type":"response.completed","response":{"usage":{"input_tokens":0,"input_tokens_details":{"cached_tokens":0}}}}),
                CacheHit::Unknown,
            ),
            (
                json!({"type":"response.completed","response":{"usage":{"input_tokens":-1,"input_tokens_details":{"cached_tokens":"0"}}}}),
                CacheHit::Unknown,
            ),
        ] {
            let turn = observer.begin(Some(&id), Instant::now());
            turn.configure(PromptCacheKeyMode::Session, "synthetic-prompt-canary", true);
            turn.terminal(&event);
            turn.terminal(&event);
            drop(turn);
            let record = receiver.recv().unwrap();
            assert_eq!(record.cache_hit, expected);
            assert_eq!(record.model, Model::Other);
            assert_eq!(record.outcome, Outcome::Success);
            assert!(receiver.try_recv().is_err());
        }
        for outcome in [
            Outcome::Failure,
            Outcome::Cancelled,
            Outcome::MissingTerminal,
            Outcome::Excluded,
        ] {
            let turn = observer.begin(Some(&id), Instant::now());
            if outcome != Outcome::Cancelled {
                turn.end(outcome);
            }
            drop(turn);
            assert_eq!(receiver.recv().unwrap().outcome, outcome);
            let next = observer.begin(Some(&id), Instant::now());
            assert_eq!(
                next.0.pending.lock().unwrap().as_ref().unwrap().record.gap,
                Bucket::Unknown
            );
            drop(next);
            receiver.recv().unwrap();
        }
        for (detail, origin, expected) in [
            (
                "http_response_sse",
                super::super::client::CodexErrorOrigin::Http,
                Outcome::MissingTerminal,
            ),
            (
                super::super::websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL,
                super::super::client::CodexErrorOrigin::WebSocket,
                Outcome::MissingTerminal,
            ),
            (
                "http_response_body",
                super::super::client::CodexErrorOrigin::Http,
                Outcome::MissingTerminal,
            ),
            (
                "http_response_headers",
                super::super::client::CodexErrorOrigin::Http,
                Outcome::Failure,
            ),
        ] {
            let turn = observer.begin(Some(&id), Instant::now());
            turn.error(&super::super::client::CodexError {
                status: 0,
                message: "raw-error-canary".into(),
                detail: Some(detail.into()),
                retry_after: None,
                usage_limit: None,
                origin,
            });
            let record = receiver.recv().unwrap();
            assert_eq!(record.outcome, expected);
            assert!(
                !serde_json::to_string(&record)
                    .unwrap()
                    .contains("raw-error-canary")
            );
        }
        for (terminal, outcome) in [
            (
                json!({"type":"response.completed","response":{"usage":usage(50)}}),
                Outcome::Success,
            ),
            (
                json!({"type":"response.failed","response":{"error":{"message":"synthetic failure"}}}),
                Outcome::Failure,
            ),
        ] {
            for events in [
                vec![json!({"type":"response.output_text.delta","delta":"synthetic"})],
                vec![
                    json!({"type":"response.output_item.added", "output_index":0,
                        "item":{"type":"reasoning","id":"synthetic","encrypted_content":"opaque"}}),
                    json!({"type":"response.output_item.done", "output_index":0,
                        "item":{"type":"reasoning","id":"synthetic"}}),
                ],
            ] {
                for late_attach in [false, true] {
                    let turn = observer.begin(Some(&id), Instant::now());
                    let mut receipt = Receipt::default();
                    if !late_attach {
                        receipt.attach(&turn);
                    }
                    receipt.observe(&terminal);
                    assert!(
                        receiver.try_recv().is_err(),
                        "pre-semantic terminals remain retryable"
                    );
                    for event in &events {
                        receipt.observe(event);
                    }
                    receipt.observe(&terminal);
                    if late_attach {
                        receipt.attach(&turn);
                    }
                    drop(turn);
                    assert_eq!(receiver.recv().unwrap().outcome, outcome);
                    assert!(
                        receiver.try_recv().is_err(),
                        "delivery failure must not overwrite an accepted terminal"
                    );
                }
            }
        }
        for (body, outcome) in [
            (b"data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"partial\"}\n\n".as_slice(), Outcome::MissingTerminal),
            (b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"failed\"}}}\n\n".as_slice(), Outcome::Failure),
        ] {
            let turn = observer.begin(Some(&id), Instant::now());
            turn.buffered_error(body);
            assert_eq!(receiver.recv().unwrap().outcome, outcome);
        }
        let mut state = State::default();
        let base = Instant::now();
        let pending = state.begin(Some(&id), at(base, 10));
        let record = state.finish(pending, base, Outcome::Success, None);
        assert_eq!(record.duration, Bucket::Unknown);
        assert!(state.entries[&id].completed.is_none());
        assert_eq!(state.begin(Some(&id), base).record.gap, Bucket::Unknown);
    }

    #[cfg(unix)]
    #[test]
    fn sink_is_opt_in_private_bounded_and_refuses_existing_outputs() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("diagnostics");
        for enabled in [None, Some("false"), Some("0"), Some("invalid")] {
            assert!(Observer::configured(enabled, Some(&path)).is_none());
            assert!(!path.exists());
        }
        assert!(Observer::configured(Some("true"), None).is_none());
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (failed_tx, failed_rx) = mpsc::channel();
        let background = Observer::start_writer(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            failed_tx.send(()).unwrap();
            Err(std::io::Error::other("synthetic initialization failure"))
        })
        .unwrap();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // Bookkeeping and enqueueing must proceed while filesystem initialization is blocked.
        background.begin(None, Instant::now()).end(Outcome::Failure);
        release_tx.send(()).unwrap();
        failed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut shared = SharedObserver {
            directory: None,
            observer: None,
        };
        let invalid = root.path().join("missing-parent").join("diagnostics");
        let first = shared.get(invalid.clone()).unwrap();
        assert!(Arc::ptr_eq(&first, &shared.get(invalid).unwrap()));
        let mut sink = Sink::create(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            sink.file.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(Sink::create(&path).is_err());
        let base = Instant::now();
        let mut state = State::default();
        let pending = state.begin(None, base);
        let record = state.finish(pending, base, Outcome::Success, Some(&usage(0)));
        for _ in 0..10000 {
            sink.write(&record).unwrap();
        }
        let files = std::fs::read_dir(&path)
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 2);
        for file in files {
            assert!(file.metadata().unwrap().len() <= MAX_BYTES);
        }
        let held = root.path().join("held");
        let victim = root.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        for name in ["turns.jsonl", "turns.previous.jsonl"] {
            std::fs::write(victim.join(name), "must survive rotation").unwrap();
        }
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::rename(&path, &held).unwrap();
        symlink(&victim, &path).unwrap();
        assert!(Sink::create(&path.join("nested")).is_err());
        for _ in 0..10000 {
            sink.write(&record).unwrap();
        }
        for name in ["turns.jsonl", "turns.previous.jsonl"] {
            assert_eq!(
                std::fs::read_to_string(victim.join(name)).unwrap(),
                "must survive rotation"
            );
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&held, &path).unwrap();
        let active = path.join("turns.jsonl");
        let existing = root.path().join("existing");
        std::fs::write(&existing, "must survive").unwrap();
        assert!(Sink::create(&existing).is_err());
        let link = root.path().join("link");
        symlink(&existing, &link).unwrap();
        assert!(Sink::create(&link).is_err());
        std::fs::remove_file(&active).unwrap();
        symlink(&existing, &active).unwrap();
        assert!(sink.write(&record).is_err());
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "must survive");
        let (observer, receiver) = observer();
        drop(receiver);
        observer.begin(None, base).end(Outcome::Failure);
    }
}

#[cfg(not(unix))]
struct Sink;
#[cfg(not(unix))]
impl Sink {
    fn create(_: &Path) -> std::io::Result<Self> {
        Err(std::io::Error::other(
            "private diagnostic sink requires Unix permissions",
        ))
    }
    fn write(&mut self, _: &Record) -> std::io::Result<()> {
        Err(std::io::Error::other("diagnostics unavailable"))
    }
}
