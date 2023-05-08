#[cfg(all(not(target_os = "windows"), target_arch = "x86_64"))]
use solana_rbpf::error::EbpfError;
use {
    crate::{invoke_context::InvokeContext, timings::ExecuteDetailsTimings},
    itertools::Itertools,
    percentage::PercentageInteger,
    solana_measure::measure::Measure,
    solana_rbpf::{
        elf::Executable,
        verifier::RequisiteVerifier,
        vm::{BuiltInProgram, VerifiedExecutable},
    },
    solana_sdk::{
        bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, clock::Slot, loader_v3,
        pubkey::Pubkey, saturating_add_assign,
    },
    std::{
        collections::HashMap,
        fmt::{Debug, Formatter},
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
    },
};

const MAX_LOADED_ENTRY_COUNT: usize = 256;
pub const DELAY_VISIBILITY_SLOT_OFFSET: Slot = 1;

/// Relationship between two fork IDs
#[derive(Copy, Clone, PartialEq)]
pub enum BlockRelation {
    /// The slot is on the same fork and is an ancestor of the other slot
    Ancestor,
    /// The two slots are equal and are on the same fork
    Equal,
    /// The slot is on the same fork and is a descendant of the other slot
    Descendant,
    /// The slots are on two different forks and may have had a common ancestor at some point
    Unrelated,
    /// Either one or both of the slots are either older than the latest root, or are in future
    Unknown,
}

/// Maps relationship between two slots.
pub trait ForkGraph {
    /// Returns the BlockRelation of A to B
    fn relationship(&self, a: Slot, b: Slot) -> BlockRelation;
}

/// Provides information about current working slot, and its ancestors
pub trait WorkingSlot {
    /// Returns the current slot value
    fn current_slot(&self) -> Slot;

    /// Returns true if the `other` slot is an ancestor of self, false otherwise
    fn is_ancestor(&self, other: Slot) -> bool;
}

#[derive(Default)]
pub enum LoadedProgramType {
    /// Tombstone for undeployed, closed or unloadable programs
    #[default]
    FailedVerification,
    Closed,
    DelayVisibility,
    /// Successfully verified but not currently compiled, used to track usage statistics when a compiled program is evicted from memory.
    Unloaded,
    LegacyV0(VerifiedExecutable<RequisiteVerifier, InvokeContext<'static>>),
    LegacyV1(VerifiedExecutable<RequisiteVerifier, InvokeContext<'static>>),
    Typed(VerifiedExecutable<RequisiteVerifier, InvokeContext<'static>>),
    #[cfg(test)]
    TestLoaded,
    Builtin(String, BuiltInProgram<InvokeContext<'static>>),
}

impl Debug for LoadedProgramType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadedProgramType::FailedVerification => {
                write!(f, "LoadedProgramType::FailedVerification")
            }
            LoadedProgramType::Closed => write!(f, "LoadedProgramType::Closed"),
            LoadedProgramType::DelayVisibility => write!(f, "LoadedProgramType::DelayVisibility"),
            LoadedProgramType::Unloaded => write!(f, "LoadedProgramType::Unloaded"),
            LoadedProgramType::LegacyV0(_) => write!(f, "LoadedProgramType::LegacyV0"),
            LoadedProgramType::LegacyV1(_) => write!(f, "LoadedProgramType::LegacyV1"),
            LoadedProgramType::Typed(_) => write!(f, "LoadedProgramType::Typed"),
            #[cfg(test)]
            LoadedProgramType::TestLoaded => write!(f, "LoadedProgramType::TestLoaded"),
            LoadedProgramType::Builtin(name, _) => {
                write!(f, "LoadedProgramType::Builtin({name})")
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct LoadedProgram {
    /// The program of this entry
    pub program: LoadedProgramType,
    /// Size of account that stores the program and program data
    pub account_size: usize,
    /// Slot in which the program was (re)deployed
    pub deployment_slot: Slot,
    /// Slot in which this entry will become active (can be in the future)
    pub effective_slot: Slot,
    /// Optional expiration slot for this entry, after which it is treated as non-existent
    pub maybe_expiration_slot: Option<Slot>,
    /// How often this entry was used
    pub usage_counter: AtomicU64,
}

#[derive(Debug, Default)]
pub struct LoadProgramMetrics {
    pub program_id: String,
    pub register_syscalls_us: u64,
    pub load_elf_us: u64,
    pub verify_code_us: u64,
    pub jit_compile_us: u64,
}

impl LoadProgramMetrics {
    pub fn submit_datapoint(&self, timings: &mut ExecuteDetailsTimings) {
        saturating_add_assign!(
            timings.create_executor_register_syscalls_us,
            self.register_syscalls_us
        );
        saturating_add_assign!(timings.create_executor_load_elf_us, self.load_elf_us);
        saturating_add_assign!(timings.create_executor_verify_code_us, self.verify_code_us);
        saturating_add_assign!(timings.create_executor_jit_compile_us, self.jit_compile_us);
        datapoint_trace!(
            "create_executor_trace",
            ("program_id", self.program_id, String),
            ("register_syscalls_us", self.register_syscalls_us, i64),
            ("load_elf_us", self.load_elf_us, i64),
            ("verify_code_us", self.verify_code_us, i64),
            ("jit_compile_us", self.jit_compile_us, i64),
        );
    }
}

impl PartialEq for LoadedProgram {
    fn eq(&self, other: &Self) -> bool {
        self.effective_slot == other.effective_slot
            && self.deployment_slot == other.deployment_slot
            && self.is_tombstone() == other.is_tombstone()
    }
}

impl LoadedProgram {
    /// Creates a new user program
    pub fn new(
        loader_key: &Pubkey,
        loader: Arc<BuiltInProgram<InvokeContext<'static>>>,
        deployment_slot: Slot,
        effective_slot: Slot,
        maybe_expiration_slot: Option<Slot>,
        elf_bytes: &[u8],
        account_size: usize,
        metrics: &mut LoadProgramMetrics,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut load_elf_time = Measure::start("load_elf_time");
        let executable = Executable::load(elf_bytes, loader.clone())?;
        load_elf_time.stop();
        metrics.load_elf_us = load_elf_time.as_us();

        let mut verify_code_time = Measure::start("verify_code_time");

        // Allowing mut here, since it may be needed for jit compile, which is under a config flag
        #[allow(unused_mut)]
        let mut program = if bpf_loader_deprecated::check_id(loader_key) {
            LoadedProgramType::LegacyV0(VerifiedExecutable::from_executable(executable)?)
        } else if bpf_loader::check_id(loader_key) || bpf_loader_upgradeable::check_id(loader_key) {
            LoadedProgramType::LegacyV1(VerifiedExecutable::from_executable(executable)?)
        } else if loader_v3::check_id(loader_key) {
            LoadedProgramType::Typed(VerifiedExecutable::from_executable(executable)?)
        } else {
            panic!();
        };
        verify_code_time.stop();
        metrics.verify_code_us = verify_code_time.as_us();

        #[cfg(all(not(target_os = "windows"), target_arch = "x86_64"))]
        {
            let mut jit_compile_time = Measure::start("jit_compile_time");
            match &mut program {
                LoadedProgramType::LegacyV0(executable) => executable.jit_compile(),
                LoadedProgramType::LegacyV1(executable) => executable.jit_compile(),
                LoadedProgramType::Typed(executable) => executable.jit_compile(),
                _ => Err(EbpfError::JitNotCompiled),
            }?;
            jit_compile_time.stop();
            metrics.jit_compile_us = jit_compile_time.as_us();
        }

        Ok(Self {
            deployment_slot,
            account_size,
            effective_slot,
            maybe_expiration_slot,
            usage_counter: AtomicU64::new(0),
            program,
        })
    }

    pub fn to_unloaded(&self) -> Self {
        Self {
            program: LoadedProgramType::Unloaded,
            account_size: self.account_size,
            deployment_slot: self.deployment_slot,
            effective_slot: self.effective_slot,
            maybe_expiration_slot: self.maybe_expiration_slot,
            usage_counter: AtomicU64::new(self.usage_counter.load(Ordering::Relaxed)),
        }
    }

    /// Creates a new built-in program
    pub fn new_builtin(
        name: String,
        deployment_slot: Slot,
        program: BuiltInProgram<InvokeContext<'static>>,
    ) -> Self {
        Self {
            deployment_slot,
            account_size: 0,
            effective_slot: deployment_slot.saturating_add(1),
            maybe_expiration_slot: None,
            usage_counter: AtomicU64::new(0),
            program: LoadedProgramType::Builtin(name, program),
        }
    }

    pub fn new_tombstone(slot: Slot, reason: LoadedProgramType) -> Self {
        let maybe_expiration_slot = matches!(reason, LoadedProgramType::DelayVisibility)
            .then_some(slot.saturating_add(DELAY_VISIBILITY_SLOT_OFFSET));
        let tombstone = Self {
            program: reason,
            account_size: 0,
            deployment_slot: slot,
            effective_slot: slot,
            maybe_expiration_slot,
            usage_counter: AtomicU64::default(),
        };
        debug_assert!(tombstone.is_tombstone());
        tombstone
    }

    pub fn is_tombstone(&self) -> bool {
        matches!(
            self.program,
            LoadedProgramType::FailedVerification
                | LoadedProgramType::Closed
                | LoadedProgramType::DelayVisibility
        )
    }

    fn is_implicit_delay_visibility_tombstone(&self, slot: Slot) -> bool {
        self.effective_slot.saturating_sub(self.deployment_slot) == DELAY_VISIBILITY_SLOT_OFFSET
            && slot >= self.deployment_slot
            && slot < self.effective_slot
    }
}

#[derive(Debug, Default)]
pub struct LoadedPrograms {
    /// A two level index:
    ///
    /// Pubkey is the address of a program, multiple versions can coexists simultaneously under the same address (in different slots).
    entries: HashMap<Pubkey, Vec<Arc<LoadedProgram>>>,

    latest_root: Slot,
}

#[derive(Debug, Default)]
pub struct LoadedProgramsForTxBatch {
    /// Pubkey is the address of a program.
    /// LoadedProgram is the corresponding program entry valid for the slot in which a transaction is being executed.
    entries: HashMap<Pubkey, Arc<LoadedProgram>>,
    slot: Slot,
}

impl LoadedProgramsForTxBatch {
    /// Refill the cache with a single entry. It's typically called during transaction loading, and
    /// transaction processing (for program management instructions).
    /// The replaces the existing entry (if any) with the provided entry. The return value contains
    /// `true` if an entry existed.
    /// The function also returns the newly inserted value.
    pub fn replenish(
        &mut self,
        key: Pubkey,
        entry: Arc<LoadedProgram>,
    ) -> (bool, Arc<LoadedProgram>) {
        debug_assert!(entry.effective_slot <= self.slot);
        (self.entries.insert(key, entry.clone()).is_some(), entry)
    }

    pub fn find(&self, key: &Pubkey) -> Option<Arc<LoadedProgram>> {
        self.entries.get(key).map(|entry| {
            if entry.is_implicit_delay_visibility_tombstone(self.slot) {
                // Found a program entry on the current fork, but it's not effective
                // yet. It indicates that the program has delayed visibility. Return
                // the tombstone to reflect that.
                Arc::new(LoadedProgram::new_tombstone(
                    entry.deployment_slot,
                    LoadedProgramType::DelayVisibility,
                ))
            } else {
                entry.clone()
            }
        })
    }

    pub fn slot(&self) -> Slot {
        self.slot
    }
}

pub enum LoadedProgramMatchCriteria {
    DeployedOnOrAfterSlot(Slot),
    Closed,
    NoCriteria,
}

impl LoadedPrograms {
    /// Refill the cache with a single entry. It's typically called during transaction loading,
    /// when the cache doesn't contain the entry corresponding to program `key`.
    /// The function dedupes the cache, in case some other thread replenished the entry in parallel.
    pub fn replenish(
        &mut self,
        key: Pubkey,
        entry: Arc<LoadedProgram>,
    ) -> (bool, Arc<LoadedProgram>) {
        let second_level = self.entries.entry(key).or_insert_with(Vec::new);
        let index = second_level
            .iter()
            .position(|at| at.effective_slot >= entry.effective_slot);
        if let Some((existing, entry_index)) =
            index.and_then(|index| second_level.get(index).map(|value| (value, index)))
        {
            if existing.deployment_slot == entry.deployment_slot
                && existing.effective_slot == entry.effective_slot
            {
                if matches!(existing.program, LoadedProgramType::Unloaded) {
                    // The unloaded program is getting reloaded
                    // Copy over the usage counter to the new entry
                    entry.usage_counter.store(
                        existing.usage_counter.load(Ordering::Relaxed),
                        Ordering::Relaxed,
                    );
                    second_level.swap_remove(entry_index);
                } else if existing.is_tombstone() && !entry.is_tombstone() {
                    // The old entry is tombstone and the new one is not. Let's give the new entry
                    // a chance.
                    second_level.swap_remove(entry_index);
                } else {
                    return (true, existing.clone());
                }
            }
        }
        second_level.insert(index.unwrap_or(second_level.len()), entry.clone());
        (false, entry)
    }

    /// Assign the program `entry` to the given `key` in the cache.
    /// This is typically called when a deployed program is managed (un-/re-/deployed) via
    /// loader instructions. Because of the cooldown, entires can not have the same
    /// deployment_slot and effective_slot.
    pub fn assign_program(&mut self, key: Pubkey, entry: Arc<LoadedProgram>) -> Arc<LoadedProgram> {
        let (was_occupied, entry) = self.replenish(key, entry);
        debug_assert!(!was_occupied);
        entry
    }

    /// Before rerooting the blockstore this removes all programs of orphan forks
    pub fn prune<F: ForkGraph>(&mut self, fork_graph: &F, new_root: Slot) {
        self.entries.retain(|_key, second_level| {
            let mut first_ancestor_found = false;
            *second_level = second_level
                .iter()
                .rev()
                .filter(|entry| {
                    let relation = fork_graph.relationship(entry.deployment_slot, new_root);
                    if entry.deployment_slot >= new_root {
                        matches!(relation, BlockRelation::Equal | BlockRelation::Descendant)
                    } else if !first_ancestor_found && matches!(relation, BlockRelation::Ancestor) {
                        first_ancestor_found = true;
                        first_ancestor_found
                    } else {
                        false
                    }
                })
                .cloned()
                .collect();
            second_level.reverse();
            !second_level.is_empty()
        });

        self.remove_expired_entries(new_root);
        self.remove_programs_with_no_entries();

        self.latest_root = std::cmp::max(self.latest_root, new_root);
    }

    fn matches_loaded_program(
        program: &Arc<LoadedProgram>,
        criteria: &LoadedProgramMatchCriteria,
    ) -> bool {
        match criteria {
            LoadedProgramMatchCriteria::DeployedOnOrAfterSlot(slot) => {
                program.deployment_slot >= *slot
            }
            LoadedProgramMatchCriteria::Closed => {
                matches!(program.program, LoadedProgramType::Closed)
            }
            LoadedProgramMatchCriteria::NoCriteria => true,
        }
    }

    /// Extracts a subset of the programs relevant to a transaction batch
    /// and returns which program accounts the accounts DB needs to load.
    pub fn extract<S: WorkingSlot>(
        &self,
        working_slot: &S,
        keys: impl Iterator<Item = (Pubkey, LoadedProgramMatchCriteria)>,
    ) -> (LoadedProgramsForTxBatch, Vec<Pubkey>) {
        let mut missing = Vec::new();
        let found = keys
            .filter_map(|(key, match_criteria)| {
                if let Some(second_level) = self.entries.get(&key) {
                    for entry in second_level.iter().rev() {
                        let current_slot = working_slot.current_slot();
                        if entry.deployment_slot <= self.latest_root
                            || entry.deployment_slot == current_slot
                            || working_slot.is_ancestor(entry.deployment_slot)
                        {
                            if entry
                                .maybe_expiration_slot
                                .map(|expiration_slot| current_slot >= expiration_slot)
                                .unwrap_or(false)
                            {
                                // Found an entry that's already expired. Any further entries in the list
                                // are older than the current one. So treat the program as missing in the
                                // cache and return early.
                                missing.push(key);
                                return None;
                            }

                            if !Self::matches_loaded_program(entry, &match_criteria) {
                                missing.push(key);
                                return None;
                            }

                            if current_slot >= entry.effective_slot {
                                return Some((key, entry.clone()));
                            } else if entry.is_implicit_delay_visibility_tombstone(current_slot) {
                                // Found a program entry on the current fork, but it's not effective
                                // yet. It indicates that the program has delayed visibility. Return
                                // the tombstone to reflect that.
                                return Some((
                                    key,
                                    Arc::new(LoadedProgram::new_tombstone(
                                        entry.deployment_slot,
                                        LoadedProgramType::DelayVisibility,
                                    )),
                                ));
                            }
                        }
                    }
                }
                missing.push(key);
                None
            })
            .collect();
        (
            LoadedProgramsForTxBatch {
                entries: found,
                slot: working_slot.current_slot(),
            },
            missing,
        )
    }

    /// Unloads programs which were used infrequently
    pub fn sort_and_unload(&mut self, shrink_to: PercentageInteger) {
        let sorted_candidates: Vec<(Pubkey, Arc<LoadedProgram>)> = self
            .entries
            .iter()
            .flat_map(|(id, list)| {
                list.iter()
                    .filter_map(move |program| match program.program {
                        LoadedProgramType::LegacyV0(_)
                        | LoadedProgramType::LegacyV1(_)
                        | LoadedProgramType::Typed(_) => Some((*id, program.clone())),
                        #[cfg(test)]
                        LoadedProgramType::TestLoaded => Some((*id, program.clone())),
                        LoadedProgramType::Unloaded
                        | LoadedProgramType::FailedVerification
                        | LoadedProgramType::Closed
                        | LoadedProgramType::DelayVisibility
                        | LoadedProgramType::Builtin(_, _) => None,
                    })
            })
            .sorted_by_cached_key(|(_id, program)| program.usage_counter.load(Ordering::Relaxed))
            .collect();

        let num_to_unload = sorted_candidates
            .len()
            .saturating_sub(shrink_to.apply_to(MAX_LOADED_ENTRY_COUNT));
        self.unload_program_entries(sorted_candidates.iter().take(num_to_unload));
        self.remove_programs_with_no_entries();
    }

    /// Removes all the entries at the given keys, if they exist
    pub fn remove_programs(&mut self, keys: impl Iterator<Item = Pubkey>) {
        for k in keys {
            self.entries.remove(&k);
        }
    }

    fn remove_expired_entries(&mut self, current_slot: Slot) {
        for entry in self.entries.values_mut() {
            entry.retain(|program| {
                program
                    .maybe_expiration_slot
                    .map(|expiration| expiration > current_slot)
                    .unwrap_or(true)
            });
        }
    }

    fn unload_program_entries<'a>(
        &mut self,
        remove: impl Iterator<Item = &'a (Pubkey, Arc<LoadedProgram>)>,
    ) {
        for (id, program) in remove {
            if let Some(entries) = self.entries.get_mut(id) {
                if let Some(candidate) = entries.iter_mut().find(|entry| entry == &program) {
                    *candidate = Arc::new(candidate.to_unloaded());
                }
            }
        }
    }

    fn remove_programs_with_no_entries(&mut self) {
        self.entries.retain(|_, programs| !programs.is_empty())
    }
}

#[cfg(RUSTC_WITH_SPECIALIZATION)]
impl solana_frozen_abi::abi_example::AbiExample for LoadedProgram {
    fn example() -> Self {
        // LoadedProgram isn't serializable by definition.
        Self::default()
    }
}

#[cfg(RUSTC_WITH_SPECIALIZATION)]
impl solana_frozen_abi::abi_example::AbiExample for LoadedPrograms {
    fn example() -> Self {
        // LoadedPrograms isn't serializable by definition.
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::loaded_programs::{
            BlockRelation, ForkGraph, LoadedProgram, LoadedProgramMatchCriteria, LoadedProgramType,
            LoadedPrograms, LoadedProgramsForTxBatch, WorkingSlot, DELAY_VISIBILITY_SLOT_OFFSET,
        },
        percentage::Percentage,
        solana_rbpf::vm::BuiltInProgram,
        solana_sdk::{clock::Slot, pubkey::Pubkey},
        std::{
            ops::ControlFlow,
            sync::{
                atomic::{AtomicU64, Ordering},
                Arc,
            },
        },
    };

    fn new_test_builtin_program(deployment_slot: Slot, effective_slot: Slot) -> Arc<LoadedProgram> {
        Arc::new(LoadedProgram {
            program: LoadedProgramType::Builtin("mockup".to_string(), BuiltInProgram::default()),
            account_size: 0,
            deployment_slot,
            effective_slot,
            maybe_expiration_slot: None,
            usage_counter: AtomicU64::default(),
        })
    }

    fn set_tombstone(
        cache: &mut LoadedPrograms,
        key: Pubkey,
        slot: Slot,
        reason: LoadedProgramType,
    ) -> Arc<LoadedProgram> {
        cache.assign_program(key, Arc::new(LoadedProgram::new_tombstone(slot, reason)))
    }

    fn insert_unloaded_program(
        cache: &mut LoadedPrograms,
        key: Pubkey,
        slot: Slot,
    ) -> Arc<LoadedProgram> {
        let unloaded =
            Arc::new(new_test_loaded_program(slot, slot.saturating_add(1)).to_unloaded());
        cache.replenish(key, unloaded).1
    }

    fn num_matching_entries<P>(cache: &LoadedPrograms, predicate: P) -> usize
    where
        P: Fn(&LoadedProgramType) -> bool,
    {
        cache
            .entries
            .values()
            .map(|programs| {
                programs
                    .iter()
                    .filter(|program| predicate(&program.program))
                    .count()
            })
            .sum()
    }

    #[test]
    fn test_eviction() {
        let mut programs = vec![];
        let mut num_total_programs: usize = 0;

        let mut cache = LoadedPrograms::default();

        let program1 = Pubkey::new_unique();
        let program1_deployment_slots = vec![0, 10, 20];
        let program1_usage_counters = vec![4, 5, 25];
        program1_deployment_slots
            .iter()
            .enumerate()
            .for_each(|(i, deployment_slot)| {
                let usage_counter = *program1_usage_counters.get(i).unwrap_or(&0);
                cache.replenish(
                    program1,
                    new_test_loaded_program_with_usage(
                        *deployment_slot,
                        (*deployment_slot) + 2,
                        AtomicU64::new(usage_counter),
                    ),
                );
                num_total_programs += 1;
                programs.push((program1, *deployment_slot, usage_counter));
            });

        for slot in 21..31 {
            set_tombstone(
                &mut cache,
                program1,
                slot,
                LoadedProgramType::FailedVerification,
            );
        }

        for slot in 31..41 {
            insert_unloaded_program(&mut cache, program1, slot);
        }

        let program2 = Pubkey::new_unique();
        let program2_deployment_slots = vec![5, 11];
        let program2_usage_counters = vec![0, 2];
        program2_deployment_slots
            .iter()
            .enumerate()
            .for_each(|(i, deployment_slot)| {
                let usage_counter = *program2_usage_counters.get(i).unwrap_or(&0);
                cache.replenish(
                    program2,
                    new_test_loaded_program_with_usage(
                        *deployment_slot,
                        (*deployment_slot) + 2,
                        AtomicU64::new(usage_counter),
                    ),
                );
                num_total_programs += 1;
                programs.push((program2, *deployment_slot, usage_counter));
            });

        for slot in 21..31 {
            set_tombstone(
                &mut cache,
                program2,
                slot,
                LoadedProgramType::DelayVisibility,
            );
        }

        for slot in 31..41 {
            insert_unloaded_program(&mut cache, program2, slot);
        }

        let program3 = Pubkey::new_unique();
        let program3_deployment_slots = vec![0, 5, 15];
        let program3_usage_counters = vec![100, 3, 20];
        program3_deployment_slots
            .iter()
            .enumerate()
            .for_each(|(i, deployment_slot)| {
                let usage_counter = *program3_usage_counters.get(i).unwrap_or(&0);
                cache.replenish(
                    program3,
                    new_test_loaded_program_with_usage(
                        *deployment_slot,
                        (*deployment_slot) + 2,
                        AtomicU64::new(usage_counter),
                    ),
                );
                num_total_programs += 1;
                programs.push((program3, *deployment_slot, usage_counter));
            });

        for slot in 21..31 {
            set_tombstone(&mut cache, program3, slot, LoadedProgramType::Closed);
        }

        for slot in 31..41 {
            insert_unloaded_program(&mut cache, program3, slot);
        }

        programs.sort_by_key(|(_id, _slot, usage_count)| *usage_count);

        let num_loaded = num_matching_entries(&cache, |program_type| {
            matches!(program_type, LoadedProgramType::TestLoaded)
        });
        let num_unloaded = num_matching_entries(&cache, |program_type| {
            matches!(program_type, LoadedProgramType::Unloaded)
        });
        let num_tombstones = num_matching_entries(&cache, |program_type| {
            matches!(
                program_type,
                LoadedProgramType::DelayVisibility
                    | LoadedProgramType::FailedVerification
                    | LoadedProgramType::Closed
            )
        });

        assert_eq!(num_loaded, 8);
        assert_eq!(num_unloaded, 30);
        assert_eq!(num_tombstones, 30);

        // Evicting to 2% should update cache with
        // * 5 active entries
        // * 33 unloaded entries (3 active programs will get unloaded)
        // * 30 tombstones (tombstones are not evicted)
        cache.sort_and_unload(Percentage::from(2));
        // Check that every program is still in the cache.
        programs.iter().for_each(|entry| {
            assert!(cache.entries.get(&entry.0).is_some());
        });

        let unloaded = cache
            .entries
            .iter()
            .flat_map(|(id, cached_programs)| {
                cached_programs.iter().filter_map(|program| {
                    matches!(program.program, LoadedProgramType::Unloaded)
                        .then_some((*id, program.usage_counter.load(Ordering::Relaxed)))
                })
            })
            .collect::<Vec<(Pubkey, u64)>>();

        for index in 0..3 {
            let expected = programs.get(index).expect("Missing program");
            assert!(unloaded.contains(&(expected.0, expected.2)));
        }

        let num_loaded = num_matching_entries(&cache, |program_type| {
            matches!(program_type, LoadedProgramType::TestLoaded)
        });
        let num_unloaded = num_matching_entries(&cache, |program_type| {
            matches!(program_type, LoadedProgramType::Unloaded)
        });
        let num_tombstones = num_matching_entries(&cache, |program_type| {
            matches!(
                program_type,
                LoadedProgramType::DelayVisibility
                    | LoadedProgramType::FailedVerification
                    | LoadedProgramType::Closed
            )
        });

        assert_eq!(num_loaded, 5);
        assert_eq!(num_unloaded, 33);
        assert_eq!(num_tombstones, 30);
    }

    #[test]
    fn test_usage_count_of_unloaded_program() {
        let mut cache = LoadedPrograms::default();

        let program = Pubkey::new_unique();
        let num_total_programs = 6;
        (0..num_total_programs).for_each(|i| {
            cache.replenish(
                program,
                new_test_loaded_program_with_usage(i, i + 2, AtomicU64::new(i + 10)),
            );
        });

        // This will unload the program deployed at slot 0, with usage count = 10
        cache.sort_and_unload(Percentage::from(2));

        let num_unloaded = num_matching_entries(&cache, |program_type| {
            matches!(program_type, LoadedProgramType::Unloaded)
        });
        assert_eq!(num_unloaded, 1);

        cache.entries.values().for_each(|programs| {
            programs.iter().for_each(|program| {
                if matches!(program.program, LoadedProgramType::Unloaded) {
                    // Test that the usage counter is retained for the unloaded program
                    assert_eq!(program.usage_counter.load(Ordering::Relaxed), 10);
                    assert_eq!(program.deployment_slot, 0);
                    assert_eq!(program.effective_slot, 2);
                }
            })
        });

        // Replenish the program that was just unloaded. Use 0 as the usage counter. This should be
        // updated with the usage counter from the unloaded program.
        cache.replenish(
            program,
            new_test_loaded_program_with_usage(0, 2, AtomicU64::new(0)),
        );

        cache.entries.values().for_each(|programs| {
            programs.iter().for_each(|program| {
                if matches!(program.program, LoadedProgramType::Unloaded)
                    && program.deployment_slot == 0
                    && program.effective_slot == 2
                {
                    // Test that the usage counter was correctly updated.
                    assert_eq!(program.usage_counter.load(Ordering::Relaxed), 10);
                }
            })
        });
    }

    #[test]
    fn test_replace_tombstones() {
        let mut cache = LoadedPrograms::default();
        let program1 = Pubkey::new_unique();
        set_tombstone(
            &mut cache,
            program1,
            10,
            LoadedProgramType::FailedVerification,
        );

        let loaded_program = new_test_loaded_program(10, 10);
        let (existing, program) = cache.replenish(program1, loaded_program.clone());
        assert!(!existing);
        assert_eq!(program, loaded_program);
    }

    #[test]
    fn test_tombstone() {
        let tombstone = LoadedProgram::new_tombstone(0, LoadedProgramType::FailedVerification);
        assert!(matches!(
            tombstone.program,
            LoadedProgramType::FailedVerification
        ));
        assert!(tombstone.is_tombstone());
        assert_eq!(tombstone.deployment_slot, 0);
        assert_eq!(tombstone.effective_slot, 0);

        let tombstone = LoadedProgram::new_tombstone(100, LoadedProgramType::Closed);
        assert!(matches!(tombstone.program, LoadedProgramType::Closed));
        assert!(tombstone.is_tombstone());
        assert_eq!(tombstone.deployment_slot, 100);
        assert_eq!(tombstone.effective_slot, 100);

        let mut cache = LoadedPrograms::default();
        let program1 = Pubkey::new_unique();
        let tombstone = set_tombstone(
            &mut cache,
            program1,
            10,
            LoadedProgramType::FailedVerification,
        );
        let second_level = &cache
            .entries
            .get(&program1)
            .expect("Failed to find the entry");
        assert_eq!(second_level.len(), 1);
        assert!(second_level.get(0).unwrap().is_tombstone());
        assert_eq!(tombstone.deployment_slot, 10);
        assert_eq!(tombstone.effective_slot, 10);

        // Add a program at slot 50, and a tombstone for the program at slot 60
        let program2 = Pubkey::new_unique();
        assert!(
            !cache
                .replenish(program2, new_test_builtin_program(50, 51))
                .0
        );
        let second_level = &cache
            .entries
            .get(&program2)
            .expect("Failed to find the entry");
        assert_eq!(second_level.len(), 1);
        assert!(!second_level.get(0).unwrap().is_tombstone());

        let tombstone = set_tombstone(
            &mut cache,
            program2,
            60,
            LoadedProgramType::FailedVerification,
        );
        let second_level = &cache
            .entries
            .get(&program2)
            .expect("Failed to find the entry");
        assert_eq!(second_level.len(), 2);
        assert!(!second_level.get(0).unwrap().is_tombstone());
        assert!(second_level.get(1).unwrap().is_tombstone());
        assert!(tombstone.is_tombstone());
        assert_eq!(tombstone.deployment_slot, 60);
        assert_eq!(tombstone.effective_slot, 60);
    }

    struct TestForkGraph {
        relation: BlockRelation,
    }
    impl ForkGraph for TestForkGraph {
        fn relationship(&self, _a: Slot, _b: Slot) -> BlockRelation {
            self.relation
        }
    }

    #[test]
    fn test_prune_empty() {
        let mut cache = LoadedPrograms::default();
        let fork_graph = TestForkGraph {
            relation: BlockRelation::Unrelated,
        };

        cache.prune(&fork_graph, 0);
        assert!(cache.entries.is_empty());

        cache.prune(&fork_graph, 10);
        assert!(cache.entries.is_empty());

        let fork_graph = TestForkGraph {
            relation: BlockRelation::Ancestor,
        };

        cache.prune(&fork_graph, 0);
        assert!(cache.entries.is_empty());

        cache.prune(&fork_graph, 10);
        assert!(cache.entries.is_empty());

        let fork_graph = TestForkGraph {
            relation: BlockRelation::Descendant,
        };

        cache.prune(&fork_graph, 0);
        assert!(cache.entries.is_empty());

        cache.prune(&fork_graph, 10);
        assert!(cache.entries.is_empty());

        let fork_graph = TestForkGraph {
            relation: BlockRelation::Unknown,
        };

        cache.prune(&fork_graph, 0);
        assert!(cache.entries.is_empty());

        cache.prune(&fork_graph, 10);
        assert!(cache.entries.is_empty());
    }

    #[derive(Default)]
    struct TestForkGraphSpecific {
        forks: Vec<Vec<Slot>>,
    }

    impl TestForkGraphSpecific {
        fn insert_fork(&mut self, fork: &[Slot]) {
            let mut fork = fork.to_vec();
            fork.sort();
            self.forks.push(fork)
        }
    }

    impl ForkGraph for TestForkGraphSpecific {
        fn relationship(&self, a: Slot, b: Slot) -> BlockRelation {
            match self.forks.iter().try_for_each(|fork| {
                let relation = fork
                    .iter()
                    .position(|x| *x == a)
                    .and_then(|a_pos| {
                        fork.iter().position(|x| *x == b).and_then(|b_pos| {
                            (a_pos == b_pos)
                                .then_some(BlockRelation::Equal)
                                .or_else(|| (a_pos < b_pos).then_some(BlockRelation::Ancestor))
                                .or(Some(BlockRelation::Descendant))
                        })
                    })
                    .unwrap_or(BlockRelation::Unrelated);

                if relation != BlockRelation::Unrelated {
                    return ControlFlow::Break(relation);
                }

                ControlFlow::Continue(())
            }) {
                ControlFlow::Break(relation) => relation,
                _ => BlockRelation::Unrelated,
            }
        }
    }

    struct TestWorkingSlot {
        slot: Slot,
        fork: Vec<Slot>,
        slot_pos: usize,
    }

    impl TestWorkingSlot {
        fn new(slot: Slot, fork: &[Slot]) -> Self {
            let mut fork = fork.to_vec();
            fork.sort();
            let slot_pos = fork
                .iter()
                .position(|current| *current == slot)
                .expect("The fork didn't have the slot in it");
            TestWorkingSlot {
                slot,
                fork,
                slot_pos,
            }
        }

        fn update_slot(&mut self, slot: Slot) {
            self.slot = slot;
            self.slot_pos = self
                .fork
                .iter()
                .position(|current| *current == slot)
                .expect("The fork didn't have the slot in it");
        }
    }

    impl WorkingSlot for TestWorkingSlot {
        fn current_slot(&self) -> Slot {
            self.slot
        }

        fn is_ancestor(&self, other: Slot) -> bool {
            self.fork
                .iter()
                .position(|current| *current == other)
                .map(|other_pos| other_pos < self.slot_pos)
                .unwrap_or(false)
        }
    }

    fn new_test_loaded_program(deployment_slot: Slot, effective_slot: Slot) -> Arc<LoadedProgram> {
        new_test_loaded_program_with_usage(deployment_slot, effective_slot, AtomicU64::default())
    }
    fn new_test_loaded_program_with_usage(
        deployment_slot: Slot,
        effective_slot: Slot,
        usage_counter: AtomicU64,
    ) -> Arc<LoadedProgram> {
        Arc::new(LoadedProgram {
            program: LoadedProgramType::TestLoaded,
            account_size: 0,
            deployment_slot,
            effective_slot,
            maybe_expiration_slot: None,
            usage_counter,
        })
    }

    fn match_slot(
        table: &LoadedProgramsForTxBatch,
        program: &Pubkey,
        deployment_slot: Slot,
        working_slot: Slot,
    ) -> bool {
        assert_eq!(table.slot, working_slot);
        table
            .find(program)
            .map(|entry| entry.deployment_slot == deployment_slot)
            .unwrap_or(false)
    }

    #[test]
    fn test_fork_extract_and_prune() {
        let mut cache = LoadedPrograms::default();

        // Fork graph created for the test
        //                   0
        //                 /   \
        //                10    5
        //                |     |
        //                20    11
        //                |     | \
        //                22   15  25
        //                      |   |
        //                     16  27
        //                      |
        //                     19
        //                      |
        //                     23

        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[0, 10, 20, 22]);
        fork_graph.insert_fork(&[0, 5, 11, 15, 16, 19, 21, 23]);
        fork_graph.insert_fork(&[0, 5, 11, 25, 27]);

        let program1 = Pubkey::new_unique();
        assert!(!cache.replenish(program1, new_test_loaded_program(0, 1)).0);
        assert!(!cache.replenish(program1, new_test_loaded_program(10, 11)).0);
        assert!(!cache.replenish(program1, new_test_loaded_program(20, 21)).0);

        // Test: inserting duplicate entry return pre existing entry from the cache
        assert!(cache.replenish(program1, new_test_loaded_program(20, 21)).0);

        let program2 = Pubkey::new_unique();
        assert!(!cache.replenish(program2, new_test_loaded_program(5, 6)).0);
        assert!(
            !cache
                .replenish(
                    program2,
                    new_test_loaded_program(11, 11 + DELAY_VISIBILITY_SLOT_OFFSET)
                )
                .0
        );

        let program3 = Pubkey::new_unique();
        assert!(!cache.replenish(program3, new_test_loaded_program(25, 26)).0);

        let program4 = Pubkey::new_unique();
        assert!(!cache.replenish(program4, new_test_loaded_program(0, 1)).0);
        assert!(!cache.replenish(program4, new_test_loaded_program(5, 6)).0);
        // The following is a special case, where effective slot is 3 slots in the future
        assert!(
            !cache
                .replenish(
                    program4,
                    new_test_loaded_program(15, 15 + DELAY_VISIBILITY_SLOT_OFFSET)
                )
                .0
        );

        // Current fork graph
        //                   0
        //                 /   \
        //                10    5
        //                |     |
        //                20    11
        //                |     | \
        //                22   15  25
        //                      |   |
        //                     16  27
        //                      |
        //                     19
        //                      |
        //                     23

        // Testing fork 0 - 10 - 12 - 22 with current slot at 22
        let working_slot = TestWorkingSlot::new(22, &[0, 10, 20, 22]);
        let (found, missing) = cache.extract(
            &working_slot,
            vec![
                (program1, LoadedProgramMatchCriteria::NoCriteria),
                (program2, LoadedProgramMatchCriteria::NoCriteria),
                (program3, LoadedProgramMatchCriteria::NoCriteria),
                (program4, LoadedProgramMatchCriteria::NoCriteria),
            ]
            .into_iter(),
        );

        assert!(match_slot(&found, &program1, 20, 22));
        assert!(match_slot(&found, &program4, 0, 22));

        assert!(missing.contains(&program2));
        assert!(missing.contains(&program3));

        // Testing fork 0 - 5 - 11 - 15 - 16 with current slot at 16
        let mut working_slot = TestWorkingSlot::new(15, &[0, 5, 11, 15, 16, 18, 19, 23]);
        let (found, missing) = cache.extract(
            &working_slot,
            vec![
                (program1, LoadedProgramMatchCriteria::NoCriteria),
                (program2, LoadedProgramMatchCriteria::NoCriteria),
                (program3, LoadedProgramMatchCriteria::NoCriteria),
                (program4, LoadedProgramMatchCriteria::NoCriteria),
            ]
            .into_iter(),
        );

        assert!(match_slot(&found, &program1, 0, 15));
        assert!(match_slot(&found, &program2, 11, 15));

        // The effective slot of program4 deployed in slot 15 is 19. So it should not be usable in slot 16.
        // A delay visibility tombstone should be returned here.
        let tombstone = found.find(&program4).expect("Failed to find the tombstone");
        assert!(matches!(
            tombstone.program,
            LoadedProgramType::DelayVisibility
        ));
        assert_eq!(tombstone.deployment_slot, 15);

        assert!(missing.contains(&program3));

        // Testing the same fork above, but current slot is now 18 (equal to effective slot of program4).
        working_slot.update_slot(18);
        let (found, missing) = cache.extract(
            &working_slot,
            vec![
                (program1, LoadedProgramMatchCriteria::NoCriteria),
                (program2, LoadedProgramMatchCriteria::NoCriteria),
                (program3, LoadedProgramMatchCriteria::NoCriteria),
                (program4, LoadedProgramMatchCriteria::NoCriteria),
            ]
            .into_iter(),
        );

        assert!(match_slot(&found, &program1, 0, 18));
        assert!(match_slot(&found, &program2, 11, 18));

        // The effective slot of program4 deployed in slot 15 is 18. So it should be usable in slot 18.
        assert!(match_slot(&found, &program4, 15, 18));

        assert!(missing.contains(&program3));

        // Testing the same fork above, but current slot is now 23 (future slot than effective slot of program4).
        working_slot.update_slot(23);
        let (found, missing) = cache.extract(
            &working_slot,
            vec![
                (program1, LoadedProgramMatchCriteria::NoCriteria),
                (program2, LoadedProgramMatchCriteria::NoCriteria),
                (program3, LoadedProgramMatchCriteria::NoCriteria),
                (program4, LoadedProgramMatchCriteria::NoCriteria),
            ]
            .into_iter(),
        );

        assert!(match_slot(&found, &program1, 0, 23));
        assert!(match_slot(&found, &program2, 11, 23));

        // The effective slot of program4 deployed in slot 15 is 19. So it should be usable in slot 23.
        assert!(match_slot(&found, &program4, 15, 23));

        assert!(missing.contains(&program3));

        // Testing fork 0 - 5 - 11 - 15 - 16 with current slot at 11
        let working_slot = TestWorkingSlot::new(11, &[0, 5, 11, 15, 16]);
        let (found, missing) = cache.extract(
            &working_slot,
            vec![
                (program1, LoadedProgramMatchCriteria::NoCriteria),
                (program2, LoadedProgramMatchCriteria::NoCriteria),
                (program3, LoadedProgramMatchCriteria::NoCriteria),
                (program4, LoadedProgramMatchCriteria::NoCriteria),
            ]
            .into_iter(),
        );

        assert!(match_slot(&found, &program1, 0, 11));
        // program2 was updated at slot 11, but is not effective till slot 12. The result should contain a tombstone.
        let tombstone = found.find(&program2).expect("Failed to find the tombstone");
        assert!(matches!(
            tombstone.program,
            LoadedProgramType::DelayVisibility
        ));
        assert_eq!(tombstone.deployment_slot, 11);
        assert!(match_slot(&found, &program4, 5, 11));

        assert!(missing.contains(&program3));

        // The following is a special case, where there's an expiration slot
        let test_program = Arc::new(LoadedProgram {
            program: LoadedProgramType::DelayVisibility,
            account_size: 0,
            deployment_slot: 19,
            effective_slot: 19,
            maybe_expiration_slot: Some(21),
            usage_counter: AtomicU64::default(),
        });
        assert!(!cache.replenish(program4, test_program).0);

        // Testing fork 0 - 5 - 11 - 15 - 16 - 19 - 21 - 23 with current slot at 19
        let working_slot = TestWorkingSlot::new(19, &[0, 5, 11, 15, 16, 18, 19, 21, 23]);
        let (found, missing) = cache.extract(
            &working_slot,
            vec![
                (program1, LoadedProgramMatchCriteria::NoCriteria),
                (program2, LoadedProgramMatchCriteria::NoCriteria),
                (program3, LoadedProgramMatchCriteria::NoCriteria),
                (program4, LoadedProgramMatchCriteria::NoCriteria),
            ]
            .into_iter(),
        );

        assert!(match_slot(&found, &program1, 0, 19));
        assert!(match_slot(&found, &program2, 11, 19));
        // Program4 deployed at slot 19 should not be expired yet
        assert!(match_slot(&found, &program4, 19, 19));

        assert!(missing.contains(&program3));

        // Testing fork 0 - 5 - 11 - 15 - 16 - 19 - 21 - 23 with current slot at 21
        // This would cause program4 deployed at slot 19 to be expired.
        let working_slot = TestWorkingSlot::new(21, &[0, 5, 11, 15, 16, 18, 19, 21, 23]);
        let (found, missing) = cache.extract(
            &working_slot,
            vec![
                (program1, LoadedProgramMatchCriteria::NoCriteria),
                (program2, LoadedProgramMatchCriteria::NoCriteria),
                (program3, LoadedProgramMatchCriteria::NoCriteria),
                (program4, LoadedProgramMatchCriteria::NoCriteria),
            ]
            .into_iter(),
        );

        assert!(match_slot(&found, &program1, 0, 21));
        assert!(match_slot(&found, &program2, 11, 21));

        assert!(missing.contains(&program3));
        assert!(missing.contains(&program4));

        // Remove the expired entry to let the rest of the test continue
        if let Some(programs) = cache.entries.get_mut(&program4) {
            programs.pop();
        }

        cache.prune(&fork_graph, 5);

        // Fork graph after pruning
        //                   0
        //                   |
        //                   5
        //                   |
        //                   11
        //                   | \
        //                  15  25
        //                   |   |
        //                  16  27
        //                   |
        //                  19
        //                   |
        //                  23

        // Testing fork 11 - 15 - 16- 19 - 22 with root at 5 and current slot at 22
        let working_slot = TestWorkingSlot::new(22, &[5, 11, 15, 16, 19, 22, 23]);
        let (found, missing) = cache.extract(
            &working_slot,
            vec![
                (program1, LoadedProgramMatchCriteria::NoCriteria),
                (program2, LoadedProgramMatchCriteria::NoCriteria),
                (program3, LoadedProgramMatchCriteria::NoCriteria),
                (program4, LoadedProgramMatchCriteria::NoCriteria),
            ]
            .into_iter(),
        );

        // Since the fork was pruned, we should not find the entry deployed at slot 20.
        assert!(match_slot(&found, &program1, 0, 22));
        assert!(match_slot(&found, &program2, 11, 22));
        assert!(match_slot(&found, &program4, 15, 22));

        assert!(missing.contains(&program3));

        // Testing fork 0 - 5 - 11 - 25 - 27 with current slot at 27
        let working_slot = TestWorkingSlot::new(27, &[11, 25, 27]);
        let (found, _missing) = cache.extract(
            &working_slot,
            vec![
                (program1, LoadedProgramMatchCriteria::NoCriteria),
                (program2, LoadedProgramMatchCriteria::NoCriteria),
                (program3, LoadedProgramMatchCriteria::NoCriteria),
                (program4, LoadedProgramMatchCriteria::NoCriteria),
            ]
            .into_iter(),
        );

        assert!(match_slot(&found, &program1, 0, 27));
        assert!(match_slot(&found, &program2, 11, 27));
        assert!(match_slot(&found, &program3, 25, 27));
        assert!(match_slot(&found, &program4, 5, 27));

        cache.prune(&fork_graph, 15);

        // Fork graph after pruning
        //                  0
        //                  |
        //                  5
        //                  |
        //                  11
        //                  |
        //                  15
        //                  |
        //                  16
        //                  |
        //                  19
        //                  |
        //                  23

        // Testing fork 16, 19, 23, with root at 15, current slot at 23
        let working_slot = TestWorkingSlot::new(23, &[16, 19, 23]);
        let (found, missing) = cache.extract(
            &working_slot,
            vec![
                (program1, LoadedProgramMatchCriteria::NoCriteria),
                (program2, LoadedProgramMatchCriteria::NoCriteria),
                (program3, LoadedProgramMatchCriteria::NoCriteria),
                (program4, LoadedProgramMatchCriteria::NoCriteria),
            ]
            .into_iter(),
        );

        assert!(match_slot(&found, &program1, 0, 23));
        assert!(match_slot(&found, &program2, 11, 23));
        assert!(match_slot(&found, &program4, 15, 23));

        // program3 was deployed on slot 25, which has been pruned
        assert!(missing.contains(&program3));
    }

    #[test]
    fn test_extract_using_deployment_slot() {
        let mut cache = LoadedPrograms::default();

        // Fork graph created for the test
        //                   0
        //                 /   \
        //                10    5
        //                |     |
        //                20    11
        //                |     | \
        //                22   15  25
        //                      |   |
        //                     16  27
        //                      |
        //                     19
        //                      |
        //                     23

        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[0, 10, 20, 22]);
        fork_graph.insert_fork(&[0, 5, 11, 15, 16, 19, 21, 23]);
        fork_graph.insert_fork(&[0, 5, 11, 25, 27]);

        let program1 = Pubkey::new_unique();
        assert!(!cache.replenish(program1, new_test_loaded_program(0, 1)).0);
        assert!(!cache.replenish(program1, new_test_loaded_program(20, 21)).0);

        let program2 = Pubkey::new_unique();
        assert!(!cache.replenish(program2, new_test_loaded_program(5, 6)).0);
        assert!(!cache.replenish(program2, new_test_loaded_program(11, 12)).0);

        let program3 = Pubkey::new_unique();
        assert!(!cache.replenish(program3, new_test_loaded_program(25, 26)).0);

        // Testing fork 0 - 5 - 11 - 15 - 16 - 19 - 21 - 23 with current slot at 19
        let working_slot = TestWorkingSlot::new(12, &[0, 5, 11, 12, 15, 16, 18, 19, 21, 23]);
        let (found, missing) = cache.extract(
            &working_slot,
            vec![
                (program1, LoadedProgramMatchCriteria::NoCriteria),
                (program2, LoadedProgramMatchCriteria::NoCriteria),
                (program3, LoadedProgramMatchCriteria::NoCriteria),
            ]
            .into_iter(),
        );

        assert!(match_slot(&found, &program1, 0, 12));
        assert!(match_slot(&found, &program2, 11, 12));

        assert!(missing.contains(&program3));

        // Test the same fork, but request the program modified at a later slot than what's in the cache.
        let (found, missing) = cache.extract(
            &working_slot,
            vec![
                (
                    program1,
                    LoadedProgramMatchCriteria::DeployedOnOrAfterSlot(5),
                ),
                (
                    program2,
                    LoadedProgramMatchCriteria::DeployedOnOrAfterSlot(5),
                ),
                (program3, LoadedProgramMatchCriteria::NoCriteria),
            ]
            .into_iter(),
        );

        assert!(match_slot(&found, &program2, 11, 12));

        assert!(missing.contains(&program1));
        assert!(missing.contains(&program3));
    }

    #[test]
    fn test_prune_expired() {
        let mut cache = LoadedPrograms::default();

        // Fork graph created for the test
        //                   0
        //                 /   \
        //                10    5
        //                |     |
        //                20    11
        //                |     | \
        //                22   15  25
        //                      |   |
        //                     16  27
        //                      |
        //                     19
        //                      |
        //                     23

        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[0, 10, 20, 22]);
        fork_graph.insert_fork(&[0, 5, 11, 15, 16, 19, 21, 23]);
        fork_graph.insert_fork(&[0, 5, 11, 25, 27]);

        let program1 = Pubkey::new_unique();
        assert!(!cache.replenish(program1, new_test_loaded_program(10, 11)).0);
        assert!(!cache.replenish(program1, new_test_loaded_program(20, 21)).0);

        let program2 = Pubkey::new_unique();
        assert!(!cache.replenish(program2, new_test_loaded_program(5, 6)).0);
        assert!(!cache.replenish(program2, new_test_loaded_program(11, 12)).0);

        let program3 = Pubkey::new_unique();
        assert!(!cache.replenish(program3, new_test_loaded_program(25, 26)).0);

        // The following is a special case, where there's an expiration slot
        let test_program = Arc::new(LoadedProgram {
            program: LoadedProgramType::DelayVisibility,
            account_size: 0,
            deployment_slot: 11,
            effective_slot: 11,
            maybe_expiration_slot: Some(15),
            usage_counter: AtomicU64::default(),
        });
        assert!(!cache.replenish(program1, test_program).0);

        // Testing fork 0 - 5 - 11 - 15 - 16 - 19 - 21 - 23 with current slot at 19
        let working_slot = TestWorkingSlot::new(12, &[0, 5, 11, 12, 15, 16, 18, 19, 21, 23]);
        let (found, missing) = cache.extract(
            &working_slot,
            vec![
                (program1, LoadedProgramMatchCriteria::NoCriteria),
                (program2, LoadedProgramMatchCriteria::NoCriteria),
                (program3, LoadedProgramMatchCriteria::NoCriteria),
            ]
            .into_iter(),
        );

        // Program1 deployed at slot 11 should not be expired yet
        assert!(match_slot(&found, &program1, 11, 12));
        assert!(match_slot(&found, &program2, 11, 12));

        assert!(missing.contains(&program3));

        // Testing fork 0 - 5 - 11 - 12 - 15 - 16 - 19 - 21 - 23 with current slot at 15
        // This would cause program4 deployed at slot 15 to be expired.
        let working_slot = TestWorkingSlot::new(15, &[0, 5, 11, 15, 16, 18, 19, 21, 23]);
        let (found, missing) = cache.extract(
            &working_slot,
            vec![
                (program1, LoadedProgramMatchCriteria::NoCriteria),
                (program2, LoadedProgramMatchCriteria::NoCriteria),
                (program3, LoadedProgramMatchCriteria::NoCriteria),
            ]
            .into_iter(),
        );

        assert!(match_slot(&found, &program2, 11, 15));

        assert!(missing.contains(&program1));
        assert!(missing.contains(&program3));

        // Test that the program still exists in the cache, even though it is expired.
        assert_eq!(
            cache
                .entries
                .get(&program1)
                .expect("Didn't find program1")
                .len(),
            3
        );

        // New root 5 should not evict the expired entry for program1
        cache.prune(&fork_graph, 5);
        assert_eq!(
            cache
                .entries
                .get(&program1)
                .expect("Didn't find program1")
                .len(),
            1
        );

        // New root 15 should evict the expired entry for program1
        cache.prune(&fork_graph, 15);
        assert!(cache.entries.get(&program1).is_none());
    }

    #[test]
    fn test_fork_prune_find_first_ancestor() {
        let mut cache = LoadedPrograms::default();

        // Fork graph created for the test
        //                   0
        //                 /   \
        //                10    5
        //                |
        //                20

        // Deploy program on slot 0, and slot 5.
        // Prune the fork that has slot 5. The cache should still have the program
        // deployed at slot 0.
        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[0, 10, 20]);
        fork_graph.insert_fork(&[0, 5]);

        let program1 = Pubkey::new_unique();
        assert!(!cache.replenish(program1, new_test_loaded_program(0, 1)).0);
        assert!(!cache.replenish(program1, new_test_loaded_program(5, 6)).0);

        cache.prune(&fork_graph, 10);

        let working_slot = TestWorkingSlot::new(20, &[0, 10, 20]);
        let (found, _missing) = cache.extract(
            &working_slot,
            vec![(program1, LoadedProgramMatchCriteria::NoCriteria)].into_iter(),
        );

        // The cache should have the program deployed at slot 0
        assert_eq!(
            found
                .entries
                .get(&program1)
                .expect("Did not find the program")
                .deployment_slot,
            0
        );
    }
}
