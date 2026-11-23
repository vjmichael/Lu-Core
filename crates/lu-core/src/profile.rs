//! Per-model profile, the K(α, N, T) layer, and routing.
//!
//! Lu keeps a record for each model and quantization it runs: what the model
//! handled and failed at, by kind of task, whether an augmentation such as
//! Cognitum helped, and whether the model uses memory tools unprompted.
//!
//! The record is built only from evidence Lu holds itself:
//! - task outcomes come from the effect journal (`APPLIED` passes, `FAILED`
//!   fails; anything unfinished or uncertain is refused as evidence);
//! - memory use is what the host observed in a turn (the model called a
//!   memory tool before Lu offered it, or it did not).
//!
//! Nothing a model says about itself is ever recorded. The profile informs
//! choices (memory path, budget, a stronger model, offering Cognitum); it
//! never grants authority.
//!
//! Routing: images go to a multimodal model, exact computation goes to
//! Cognitum when it is connected. Lu core does not depend on Cognitum: when it
//! is not connected the work stays with the main model, and the result is
//! said to be unchecked by an engine.

use std::{path::Path, sync::Mutex};

use rusqlite::{params, Connection, OptionalExtension};

use crate::journal::{EffectJournal, EffectState};
use crate::memory::DEFAULT_BUDGET_CHARS;

/// Observed turns considered when choosing the memory path.
pub const MEMORY_WINDOW: usize = 10;
/// Observed turns needed before a model may leave the Lu-driven path.
pub const MIN_MEMORY_OBSERVATIONS: usize = 5;
/// Share of the window (in percent) with unprompted memory use needed.
pub const MODEL_DRIVEN_PERCENT: usize = 80;
/// Budget for a model that recalls on its own: Lu injects only what the model
/// would not ask for (goal, open questions, unknown outcomes, facts).
pub const MODEL_DRIVEN_BUDGET_CHARS: usize = DEFAULT_BUDGET_CHARS / 3;
/// Verified outcomes needed on each side before the profile draws a
/// conclusion (augmentation helps, a model struggles, another does better).
pub const MIN_SAMPLES: u32 = 3;
/// Recent outcomes per task kind that make up the current record.
pub const OUTCOME_WINDOW: u32 = 20;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelRef {
    /// The model, e.g. `qwen3.6-14b`.
    pub id: String,
    /// The quantization, e.g. `Q4_K_M`. A different quantization is a
    /// different profile: its capability differs.
    pub quant: String,
}
impl ModelRef {
    pub fn new(id: &str, quant: &str) -> Self {
        Self { id: id.into(), quant: quant.into() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskKind {
    // The first kinds, from the owner's Cognitum tests. Soft reasoning:
    // Cognitum help is offered only where the profile shows it helps.
    Scheduling,
    MultiStepMath,
    ReasoningPuzzle,
    // Exact computation: an engine is better than any model, so the handoff
    // to Cognitum is the default.
    ExactArithmetic,
    SymbolicMath,
    FormalLogic,
    Optimization,
    Probability,
    // Everything else.
    Coding,
    FileWork,
    General,
}
impl TaskKind {
    pub const ALL: [TaskKind; 11] = [
        Self::Scheduling,
        Self::MultiStepMath,
        Self::ReasoningPuzzle,
        Self::ExactArithmetic,
        Self::SymbolicMath,
        Self::FormalLogic,
        Self::Optimization,
        Self::Probability,
        Self::Coding,
        Self::FileWork,
        Self::General,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scheduling => "scheduling",
            Self::MultiStepMath => "multi-step-math",
            Self::ReasoningPuzzle => "reasoning-puzzle",
            Self::ExactArithmetic => "exact-arithmetic",
            Self::SymbolicMath => "symbolic-math",
            Self::FormalLogic => "formal-logic",
            Self::Optimization => "optimization",
            Self::Probability => "probability",
            Self::Coding => "coding",
            Self::FileWork => "file-work",
            Self::General => "general",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
    /// Exact computation, handed to Cognitum by default.
    pub fn is_exact(self) -> bool {
        matches!(
            self,
            Self::ExactArithmetic | Self::SymbolicMath | Self::FormalLogic | Self::Optimization | Self::Probability
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Augmentation {
    None,
    Cognitum,
}
impl Augmentation {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Cognitum => "cognitum",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rate {
    pub passed: u32,
    pub failed: u32,
}
impl Rate {
    pub fn samples(self) -> u32 {
        self.passed + self.failed
    }
    /// `self` has a strictly higher pass rate than `other` (exact, no floats).
    pub fn better_than(self, other: Rate) -> bool {
        self.samples() > 0
            && (other.samples() == 0
                || u64::from(self.passed) * u64::from(other.samples())
                    > u64::from(other.passed) * u64::from(self.samples()))
    }
    /// Fewer than half passed.
    pub fn struggling(self) -> bool {
        self.passed * 2 < self.samples()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TaskStats {
    /// The model on its own.
    pub plain: Rate,
    /// The model with Cognitum's help.
    pub augmented: Rate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryMode {
    /// Lu retrieves and injects; the default for every model.
    LuDriven,
    /// The model saves and recalls on its own; Lu injects less.
    ModelDriven,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryPlan {
    pub mode: MemoryMode,
    /// Character budget for the injected memory block.
    pub budget_chars: usize,
    /// Turns observed in the window, and how many used memory unprompted.
    pub observed: usize,
    pub unprompted: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub model: ModelRef,
    pub record: Rate,
    pub current: Rate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteRequest {
    pub kind: TaskKind,
    /// The turn carries images.
    pub has_images: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// The current model handles it.
    Main {
        /// Cognitum's softer help is worth offering, per this model's record.
        offer_cognitum: bool,
        /// A model with a better record for this kind of task.
        suggest: Option<Suggestion>,
        /// Said to the user, e.g. that an exact result was not engine-checked.
        note: Option<String>,
    },
    /// Hand the images to this multimodal model.
    Multimodal(ModelRef),
    /// Images arrived but no multimodal model is installed. Lu says so
    /// instead of letting a text model guess at them.
    NoMultimodalModel,
    /// Hand exact computation to Cognitum.
    Cognitum,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    Database(String),
    Locked,
    UnknownModel(String),
    /// The call ID is not in the effect journal.
    NoEvidence(String),
    /// The effect has not finished, or its outcome is uncertain.
    NotVerified(String),
    /// The call ID was already counted for another model or task kind.
    AlreadyCounted(String),
    Journal(String),
}
impl From<rusqlite::Error> for ProfileError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e.to_string())
    }
}
impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ProfileError {}

pub struct ProfileStore {
    db: Mutex<Connection>,
}

impl ProfileStore {
    pub fn open(path: &Path) -> Result<Self, ProfileError> {
        let db = Connection::open(path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS lu_models (
                model_id TEXT NOT NULL,
                quant TEXT NOT NULL,
                multimodal INTEGER NOT NULL CHECK(multimodal IN (0,1)),
                registered_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY(model_id, quant)
            );
            CREATE TABLE IF NOT EXISTS lu_task_outcomes (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                model_id TEXT NOT NULL,
                quant TEXT NOT NULL,
                task_kind TEXT NOT NULL,
                augmentation TEXT NOT NULL CHECK(augmentation IN ('none','cognitum')),
                call_id TEXT NOT NULL UNIQUE,
                passed INTEGER NOT NULL CHECK(passed IN (0,1)),
                recorded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY(model_id, quant) REFERENCES lu_models(model_id, quant)
            );
            CREATE INDEX IF NOT EXISTS lu_task_outcomes_by_model
              ON lu_task_outcomes(model_id, quant, task_kind, augmentation, seq);
            CREATE TABLE IF NOT EXISTS lu_memory_use (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                model_id TEXT NOT NULL,
                quant TEXT NOT NULL,
                turn_id TEXT NOT NULL,
                unprompted INTEGER NOT NULL CHECK(unprompted IN (0,1)),
                recorded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(model_id, quant, turn_id),
                FOREIGN KEY(model_id, quant) REFERENCES lu_models(model_id, quant)
            );
            -- Evidence is never rewritten.
            CREATE TRIGGER IF NOT EXISTS lu_task_outcomes_no_update
              BEFORE UPDATE ON lu_task_outcomes BEGIN SELECT RAISE(ABORT, 'outcomes are immutable'); END;
            CREATE TRIGGER IF NOT EXISTS lu_memory_use_no_update
              BEFORE UPDATE ON lu_memory_use BEGIN SELECT RAISE(ABORT, 'observations are immutable'); END;",
        )?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self { db: Mutex::new(db) })
    }

    fn db(&self) -> Result<std::sync::MutexGuard<'_, Connection>, ProfileError> {
        self.db.lock().map_err(|_| ProfileError::Locked)
    }

    /// Register a model Lu can run. Re-registering updates whether it takes
    /// images (that is a property of the file, read by the loader).
    pub fn register_model(&self, model: &ModelRef, multimodal: bool) -> Result<(), ProfileError> {
        self.db()?.execute(
            "INSERT INTO lu_models(model_id, quant, multimodal) VALUES (?1, ?2, ?3)
             ON CONFLICT(model_id, quant) DO UPDATE SET multimodal = excluded.multimodal",
            params![model.id, model.quant, multimodal],
        )?;
        Ok(())
    }

    /// Registered models, with whether each takes images.
    pub fn models(&self) -> Result<Vec<(ModelRef, bool)>, ProfileError> {
        let db = self.db()?;
        let mut q = db.prepare("SELECT model_id, quant, multimodal FROM lu_models ORDER BY model_id, quant")?;
        let rows = q.query_map([], |r| Ok((ModelRef { id: r.get(0)?, quant: r.get(1)? }, r.get::<_, bool>(2)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn require(&self, db: &Connection, model: &ModelRef) -> Result<(), ProfileError> {
        let known: Option<i64> = db
            .query_row(
                "SELECT 1 FROM lu_models WHERE model_id = ?1 AND quant = ?2",
                params![model.id, model.quant],
                |r| r.get(0),
            )
            .optional()?;
        known.map(|_| ()).ok_or_else(|| ProfileError::UnknownModel(format!("{} {}", model.id, model.quant)))
    }

    /// Count a finished effect as this model's outcome on a kind of task.
    /// The result is read from the journal, never supplied by the caller:
    /// `APPLIED` is a pass, `FAILED` is a fail, anything else is refused.
    /// Returns whether it passed. Counting the same call again for the same
    /// model and kind is a no-op; for anything else it is refused.
    pub fn record_outcome(
        &self,
        model: &ModelRef,
        kind: TaskKind,
        augmentation: Augmentation,
        call_id: &str,
        journal: &EffectJournal,
    ) -> Result<bool, ProfileError> {
        let effect = journal
            .get(call_id)
            .map_err(|e| ProfileError::Journal(format!("{e:?}")))?
            .ok_or_else(|| ProfileError::NoEvidence(call_id.into()))?;
        let passed = match effect.state {
            EffectState::Applied => true,
            EffectState::Failed => false,
            other => return Err(ProfileError::NotVerified(format!("{call_id} is {other:?}"))),
        };
        let db = self.db()?;
        self.require(&db, model)?;
        let existing: Option<(String, String, String, String)> = db
            .query_row(
                "SELECT model_id, quant, task_kind, augmentation FROM lu_task_outcomes WHERE call_id = ?1",
                params![call_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        if let Some(row) = existing {
            let same = (model.id.clone(), model.quant.clone(), kind.as_str().to_owned(), augmentation.as_str().to_owned());
            return if row == same { Ok(passed) } else { Err(ProfileError::AlreadyCounted(call_id.into())) };
        }
        db.execute(
            "INSERT INTO lu_task_outcomes(model_id, quant, task_kind, augmentation, call_id, passed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![model.id, model.quant, kind.as_str(), augmentation.as_str(), call_id, passed],
        )?;
        Ok(passed)
    }

    /// Record what the host observed in a turn: whether the model called a
    /// memory tool (recall, save, fetch by source ID) without Lu's injection
    /// having supplied what it needed. One observation per turn.
    pub fn record_memory_use(&self, model: &ModelRef, turn_id: &str, unprompted: bool) -> Result<(), ProfileError> {
        let db = self.db()?;
        self.require(&db, model)?;
        db.execute(
            "INSERT INTO lu_memory_use(model_id, quant, turn_id, unprompted) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(model_id, quant, turn_id) DO NOTHING",
            params![model.id, model.quant, turn_id, unprompted],
        )?;
        Ok(())
    }

    fn rate(db: &Connection, model: &ModelRef, kind: TaskKind, aug: Augmentation) -> Result<Rate, ProfileError> {
        let mut q = db.prepare(
            "SELECT passed FROM lu_task_outcomes
             WHERE model_id = ?1 AND quant = ?2 AND task_kind = ?3 AND augmentation = ?4
             ORDER BY seq DESC LIMIT ?5",
        )?;
        let mut rate = Rate::default();
        for passed in q.query_map(params![model.id, model.quant, kind.as_str(), aug.as_str(), OUTCOME_WINDOW], |r| r.get::<_, bool>(0))? {
            if passed? {
                rate.passed += 1;
            } else {
                rate.failed += 1;
            }
        }
        Ok(rate)
    }

    /// The model's recent record on a kind of task, alone and with Cognitum.
    pub fn stats(&self, model: &ModelRef, kind: TaskKind) -> Result<TaskStats, ProfileError> {
        let db = self.db()?;
        Ok(TaskStats {
            plain: Self::rate(&db, model, kind, Augmentation::None)?,
            augmented: Self::rate(&db, model, kind, Augmentation::Cognitum)?,
        })
    }

    /// Which memory path this model gets, and how much Lu injects. Every
    /// model starts Lu-driven; it moves to model-driven only after enough
    /// observed turns of unprompted memory use, and moves back if it stops.
    pub fn memory_plan(&self, model: &ModelRef) -> Result<MemoryPlan, ProfileError> {
        let db = self.db()?;
        self.require(&db, model)?;
        let mut q = db.prepare(
            "SELECT unprompted FROM lu_memory_use WHERE model_id = ?1 AND quant = ?2 ORDER BY seq DESC LIMIT ?3",
        )?;
        let window: Vec<bool> = q
            .query_map(params![model.id, model.quant, MEMORY_WINDOW as i64], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let observed = window.len();
        let unprompted = window.iter().filter(|u| **u).count();
        let model_driven = observed >= MIN_MEMORY_OBSERVATIONS && unprompted * 100 >= observed * MODEL_DRIVEN_PERCENT;
        Ok(if model_driven {
            MemoryPlan { mode: MemoryMode::ModelDriven, budget_chars: MODEL_DRIVEN_BUDGET_CHARS, observed, unprompted }
        } else {
            MemoryPlan { mode: MemoryMode::LuDriven, budget_chars: DEFAULT_BUDGET_CHARS, observed, unprompted }
        })
    }

    /// Offer Cognitum's help only where this model's record shows it helps:
    /// enough samples both ways and a strictly better pass rate with it.
    pub fn offer_augmentation(&self, model: &ModelRef, kind: TaskKind) -> Result<bool, ProfileError> {
        let s = self.stats(model, kind)?;
        Ok(s.plain.samples() >= MIN_SAMPLES && s.augmented.samples() >= MIN_SAMPLES && s.augmented.better_than(s.plain))
    }

    /// A model with a better record, when this one struggles at a kind of
    /// task (enough samples, fewer than half passed). The best candidate by
    /// pass rate, with enough samples of its own.
    pub fn suggest_stronger(&self, model: &ModelRef, kind: TaskKind) -> Result<Option<Suggestion>, ProfileError> {
        let current = self.stats(model, kind)?.plain;
        if current.samples() < MIN_SAMPLES || !current.struggling() {
            return Ok(None);
        }
        let mut best: Option<Suggestion> = None;
        for (other, _) in self.models()? {
            if &other == model {
                continue;
            }
            let record = self.stats(&other, kind)?.plain;
            if record.samples() < MIN_SAMPLES || !record.better_than(current) {
                continue;
            }
            if best.as_ref().is_none_or(|b| record.better_than(b.record)) {
                best = Some(Suggestion { model: other, record, current });
            }
        }
        Ok(best)
    }

    /// Where this turn's work goes. `cognitum_connected` is whether the
    /// Cognitum extension is plugged in right now; Lu core runs without it.
    pub fn route(&self, current: &ModelRef, request: &RouteRequest, cognitum_connected: bool) -> Result<Route, ProfileError> {
        let models = self.models()?;
        if !models.iter().any(|(m, _)| m == current) {
            return Err(ProfileError::UnknownModel(format!("{} {}", current.id, current.quant)));
        }
        if request.has_images {
            if models.iter().any(|(m, mm)| m == current && *mm) {
                return Ok(Route::Multimodal(current.clone()));
            }
            // Prefer the multimodal model with the best record on this kind.
            let mut best: Option<(ModelRef, Rate)> = None;
            for (m, mm) in models {
                if !mm {
                    continue;
                }
                let rate = self.stats(&m, request.kind)?.plain;
                if best.as_ref().is_none_or(|(_, r)| rate.better_than(*r)) {
                    best = Some((m, rate));
                }
            }
            return Ok(best.map_or(Route::NoMultimodalModel, |(m, _)| Route::Multimodal(m)));
        }
        if request.kind.is_exact() {
            if cognitum_connected {
                return Ok(Route::Cognitum);
            }
            return Ok(Route::Main {
                offer_cognitum: false,
                suggest: self.suggest_stronger(current, request.kind)?,
                note: Some("Cognitum is not connected; this exact result comes from the model and was not checked by an engine.".into()),
            });
        }
        Ok(Route::Main {
            offer_cognitum: cognitum_connected && self.offer_augmentation(current, request.kind)?,
            suggest: self.suggest_stronger(current, request.kind)?,
            note: None,
        })
    }
}
