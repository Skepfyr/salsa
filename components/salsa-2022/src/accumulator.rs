//! Basic test of accumulator functionality.

use std::fmt;

use crate::{
    cycle::CycleRecoveryStrategy,
    hash::FxDashMap,
    ingredient::{fmt_index, Ingredient, IngredientRequiresReset},
    interned::Interner,
    key::DependencyIndex,
    runtime::{local_state::QueryOrigin, StampedValue},
    storage::HasJar,
    DatabaseKeyIndex, Durability, Event, EventKind, Id, IngredientIndex, Revision, Runtime,
};

pub trait Accumulator {
    type Data: Clone;
    type Jar;

    fn accumulator_ingredient<Db>(db: &Db) -> &AccumulatorIngredient<Self::Data>
    where
        Db: ?Sized + HasJar<Self::Jar>;
}

pub struct AccumulatorIngredient<Data: Clone> {
    index: IngredientIndex,
    interner: Interner<Id, DependencyIndex>,
    map: FxDashMap<DependencyIndex, AccumulatedValues<Data>>,
    debug_name: &'static str,
}

struct AccumulatedValues<Data> {
    previous: AccumulatedValuesSnapshot<Data>,
    in_progress: AccumulatedValuesSnapshot<Data>,
}

struct AccumulatedValuesSnapshot<Data> {
    produced_at: Revision,
    durability: Durability,
    values: Vec<Data>,
}

impl<Data: Clone> AccumulatorIngredient<Data> {
    pub fn new(index: IngredientIndex, debug_name: &'static str) -> Self {
        Self {
            index,
            interner: Interner::default(),
            map: FxDashMap::default(),
            debug_name,
        }
    }

    fn dependency_index(&self, key: DependencyIndex) -> DependencyIndex {
        DependencyIndex {
            ingredient_index: self.index,
            key_index: Some(self.interner.intern(key)),
        }
    }

    pub fn push(&self, runtime: &Runtime, value: Data) {
        let current_revision = runtime.current_revision();
        let (active_query, StampedValue { durability, .. }) = match runtime.active_query() {
            Some(pair) => pair,
            None => {
                panic!("cannot accumulate values outside of an active query")
            }
        };

        let mut accumulated_values =
            self.map
                .entry(active_query.into())
                .or_insert(AccumulatedValues {
                    previous: AccumulatedValuesSnapshot {
                        produced_at: current_revision,
                        durability,
                        values: vec![],
                    },
                    in_progress: AccumulatedValuesSnapshot {
                        produced_at: current_revision,
                        durability,
                        values: vec![],
                    },
                });

        // This is the first push in a new revision. Reset.
        if accumulated_values.in_progress.produced_at != current_revision {
            accumulated_values.in_progress.produced_at = current_revision;
            accumulated_values.in_progress.durability = durability;
            accumulated_values.in_progress.values.clear();
        }

        runtime.add_output(self.dependency_index(active_query.into()));
        accumulated_values.in_progress.values.push(value);
        accumulated_values.in_progress.durability =
            accumulated_values.in_progress.durability.min(durability);
    }

    pub(crate) fn produced_by(
        &self,
        runtime: &Runtime,
        query: DatabaseKeyIndex,
        output: &mut Vec<Data>,
    ) {
        let query: DependencyIndex = query.into();
        let current_revision = runtime.current_revision();
        if let Some(v) = self.map.get(&query) {
            let AccumulatedValues {
                previous,
                in_progress,
            } = v.value();

            runtime.report_tracked_read(
                self.dependency_index(query),
                in_progress.durability,
                current_revision,
            );

            if in_progress.produced_at == current_revision {
                output.extend(in_progress.values.iter().cloned());
            }
        }
    }
}

impl<DB: ?Sized, Data> Ingredient<DB> for AccumulatorIngredient<Data>
where
    DB: crate::Database,
    Data: Clone + Eq,
{
    fn maybe_changed_after(&self, db: &DB, input: DependencyIndex, revision: Revision) -> bool {
        assert_eq!(self.index, input.ingredient_index);

        let value = self.map.get(&input);
        let value = match &value {
            Some(value) => value.value(),
            None => return true,
        };

        if !db.maybe_changed_after(*self.interner.data(input.key_index.unwrap()), revision) {
            return false;
        }
        true
    }

    fn cycle_recovery_strategy(&self) -> CycleRecoveryStrategy {
        CycleRecoveryStrategy::Panic
    }

    fn origin(&self, _key_index: crate::Id) -> Option<QueryOrigin> {
        None
    }

    fn mark_validated_output(
        &self,
        db: &DB,
        executor: DatabaseKeyIndex,
        output_key: Option<crate::Id>,
    ) {
        assert!(output_key.is_some());
        let current_revision = db.runtime().current_revision();
        if let Some(mut v) = self.map.get_mut(&executor.into()) {
            // The value is still valid in the new revision.
            v.in_progress.produced_at = current_revision;
        }
    }

    fn remove_stale_output(
        &self,
        db: &DB,
        executor: DatabaseKeyIndex,
        stale_output_key: Option<crate::Id>,
    ) {
        let stale_output_key = stale_output_key.unwrap();
        let accumulator = self.dependency_index(executor.into());
        assert_eq!(accumulator.key_index.unwrap(), stale_output_key);

        if self.map.remove(&executor.into()).is_some() {
            self.interner.delete_index(stale_output_key);
            db.salsa_event(Event {
                runtime_id: db.runtime().id(),
                kind: EventKind::DidDiscardAccumulated {
                    executor_key: executor,
                    accumulator,
                },
            })
        }
    }

    fn reset_for_new_revision(&mut self) {
        self.interner.clear_deleted_indices()
    }

    fn salsa_struct_deleted(&self, _db: &DB, _id: crate::Id) {
        panic!("unexpected call: accumulator is not registered as a dependent fn");
    }

    fn fmt_index(&self, index: Option<crate::Id>, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_index(self.debug_name, index, fmt)
    }
}

impl<Data> IngredientRequiresReset for AccumulatorIngredient<Data>
where
    Data: Clone,
{
    const RESET_ON_NEW_REVISION: bool = true;
}
