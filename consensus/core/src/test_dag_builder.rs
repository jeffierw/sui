// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BTreeMap, HashSet},
    ops::{Bound::Included, RangeInclusive},
    sync::Arc,
};

use consensus_config::AuthorityIndex;
use consensus_types::block::{BlockDigest, BlockRef, BlockTimestampMs, Round, TransactionIndex};
use parking_lot::RwLock;
use rand::{rngs::StdRng, seq::SliceRandom, thread_rng, Rng, SeedableRng};

use crate::{
    block::{genesis_blocks, BlockAPI, BlockTransactionVotes, Slot, TestBlock, VerifiedBlock},
    commit::{CertifiedCommit, CommitDigest, TrustedCommit},
    context::Context,
    dag_state::DagState,
    leader_schedule::{LeaderSchedule, LeaderSwapTable},
    linearizer::{BlockStoreAPI, Linearizer},
    CommitRef, CommittedSubDag, Transaction,
};

/// DagBuilder API
///
/// Usage:
///
/// DAG Building
/// ```ignore
/// let context = Arc::new(Context::new_for_test(4).0);
/// let mut dag_builder = DagBuilder::new(context);
/// dag_builder.layer(1).build(); // Round 1 is fully connected with parents by default.
/// dag_builder.layers(2..=10).build(); // Rounds 2 ~ 10 are fully connected with parents by default.
/// dag_builder.layer(11).min_parent_links().build(); // Round 11 is minimally and randomly connected with parents, without weak links.
/// dag_builder.layer(12).no_leader_block(0).build(); // Round 12 misses leader block. Other blocks are fully connected with parents.
/// dag_builder.layer(13).no_leader_link(12, 0).build(); // Round 13 misses votes for leader block. Other blocks are fully connected with parents.
/// dag_builder.layer(14).authorities(vec![3,5]).skip_block().build(); // Round 14 authorities 3 and 5 will not propose any block.
/// dag_builder.layer(15).authorities(vec![3,5]).skip_ancestor_links(vec![1,2]).build(); // Round 15 authorities 3 and 5 will not link to ancestors 1 and 2
/// dag_builder.layer(16).authorities(vec![3,5]).equivocate(3).build(); // Round 16 authorities 3 and 5 will produce 3 equivocating blocks.
/// ```
///
/// Persisting to DagState by Layer
/// ```ignore
/// let dag_state = Arc::new(RwLock::new(DagState::new(
///    dag_builder.context.clone(),
///    Arc::new(MemStore::new()),
/// )));
/// let context = Arc::new(Context::new_for_test(4).0);
/// let mut dag_builder = DagBuilder::new(context);
/// dag_builder.layer(1).build().persist_layers(dag_state.clone()); // persist the layer
/// ```
///
/// Persisting entire DAG to DagState
/// ```ignore
/// let dag_state = Arc::new(RwLock::new(DagState::new(
///    dag_builder.context.clone(),
///    Arc::new(MemStore::new()),
/// )));
/// let context = Arc::new(Context::new_for_test(4).0);
/// let mut dag_builder = DagBuilder::new(context);
/// dag_builder.layer(1).build();
/// dag_builder.layers(2..=10).build();
/// dag_builder.persist_all_blocks(dag_state.clone()); // persist entire DAG
/// ```
///
/// Printing DAG
/// ```ignore
/// let context = Arc::new(Context::new_for_test(4).0);
/// let mut dag_builder = DagBuilder::new(context);
/// dag_builder.layer(1).build();
/// dag_builder.print(); // pretty print the entire DAG
/// ```
pub struct DagBuilder {
    pub(crate) context: Arc<Context>,
    pub(crate) leader_schedule: LeaderSchedule,
    // The genesis blocks
    pub(crate) genesis: BTreeMap<BlockRef, VerifiedBlock>,
    // The current set of ancestors that any new layer will attempt to connect to.
    pub(crate) last_ancestors: Vec<BlockRef>,
    // All blocks created by dag builder. Will be used to pretty print or to be
    // retrieved for testing/persiting to dag state.
    pub(crate) blocks: BTreeMap<BlockRef, VerifiedBlock>,
    // All the committed sub dags created by the dag builder.
    pub(crate) committed_sub_dags: Vec<(CommittedSubDag, TrustedCommit)>,
    pub(crate) last_committed_rounds: Vec<Round>,

    number_of_leaders: u32,
}

#[allow(unused)]
impl DagBuilder {
    pub fn new(context: Arc<Context>) -> Self {
        let leader_schedule = LeaderSchedule::new(context.clone(), LeaderSwapTable::default());
        let genesis_blocks = genesis_blocks(context.as_ref());
        let genesis: BTreeMap<BlockRef, VerifiedBlock> = genesis_blocks
            .into_iter()
            .map(|block| (block.reference(), block))
            .collect();
        let last_ancestors = genesis.keys().cloned().collect();
        Self {
            last_committed_rounds: vec![0; context.committee.size()],
            context,
            leader_schedule,
            number_of_leaders: 1,
            genesis,
            last_ancestors,
            blocks: BTreeMap::new(),
            committed_sub_dags: vec![],
        }
    }

    pub(crate) fn blocks(&self, rounds: RangeInclusive<Round>) -> Vec<VerifiedBlock> {
        assert!(
            !self.blocks.is_empty(),
            "No blocks have been created, please make sure that you have called build method"
        );
        self.blocks
            .iter()
            .filter_map(|(block_ref, block)| rounds.contains(&block_ref.round).then_some(block))
            .cloned()
            .collect::<Vec<VerifiedBlock>>()
    }

    pub fn all_blocks(&self) -> Vec<VerifiedBlock> {
        assert!(
            !self.blocks.is_empty(),
            "No blocks have been created, please make sure that you have called build method"
        );
        self.blocks.values().cloned().collect()
    }

    pub(crate) fn get_sub_dag_and_commits(
        &mut self,
        leader_rounds: RangeInclusive<Round>,
    ) -> Vec<(CommittedSubDag, TrustedCommit)> {
        let (last_leader_round, mut last_commit_ref, mut last_timestamp_ms) =
            if let Some((sub_dag, _)) = self.committed_sub_dags.last() {
                (
                    sub_dag.leader.round,
                    sub_dag.commit_ref,
                    sub_dag.timestamp_ms,
                )
            } else {
                (0, CommitRef::new(0, CommitDigest::MIN), 0)
            };

        struct BlockStorage {
            gc_round: Round,
            blocks: BTreeMap<BlockRef, (VerifiedBlock, bool)>, // the tuple represends the block and whether it is committed
            genesis: BTreeMap<BlockRef, VerifiedBlock>,
        }
        impl BlockStoreAPI for BlockStorage {
            fn get_blocks(&self, refs: &[BlockRef]) -> Vec<Option<VerifiedBlock>> {
                refs.iter()
                    .map(|block_ref| {
                        if block_ref.round == 0 {
                            return self.genesis.get(block_ref).cloned();
                        }
                        self.blocks
                            .get(block_ref)
                            .map(|(block, _committed)| block.clone())
                    })
                    .collect()
            }

            fn gc_round(&self) -> Round {
                self.gc_round
            }

            fn set_committed(&mut self, block_ref: &BlockRef) -> bool {
                let Some((_block, committed)) = self.blocks.get_mut(block_ref) else {
                    panic!("Block {:?} should be found in store", block_ref);
                };
                if !*committed {
                    *committed = true;
                    return true;
                }
                false
            }

            fn is_committed(&self, block_ref: &BlockRef) -> bool {
                self.blocks
                    .get(block_ref)
                    .map(|(_, committed)| *committed)
                    .expect("Block should be found in store")
            }
        }

        let mut storage = BlockStorage {
            blocks: self
                .blocks
                .clone()
                .into_iter()
                .map(|(k, v)| (k, (v, false)))
                .collect(),
            genesis: self.genesis.clone(),
            gc_round: 0,
        };

        // Create any remaining committed sub dags
        for leader_block in self
            .leader_blocks(last_leader_round + 1..=*leader_rounds.end())
            .into_iter()
            .flatten()
        {
            // set the gc round to the round of the leader block
            storage.gc_round = leader_block
                .round()
                .saturating_sub(1)
                .saturating_sub(self.context.protocol_config.gc_depth());

            let leader_block_ref = leader_block.reference();

            let to_commit = Linearizer::linearize_sub_dag(leader_block.clone(), &mut storage);

            last_timestamp_ms = Linearizer::calculate_commit_timestamp(
                &self.context.clone(),
                &mut storage,
                &leader_block,
                last_timestamp_ms,
            );

            // Update the last committed rounds
            for block in &to_commit {
                self.last_committed_rounds[block.author()] =
                    self.last_committed_rounds[block.author()].max(block.round());
            }

            let commit = TrustedCommit::new_for_test(
                last_commit_ref.index + 1,
                last_commit_ref.digest,
                last_timestamp_ms,
                leader_block_ref,
                to_commit
                    .iter()
                    .map(|block| block.reference())
                    .collect::<Vec<_>>(),
            );

            last_commit_ref = commit.reference();

            let sub_dag = CommittedSubDag::new(
                leader_block_ref,
                to_commit,
                last_timestamp_ms,
                commit.reference(),
            );

            self.committed_sub_dags.push((sub_dag, commit));
        }

        self.committed_sub_dags
            .clone()
            .into_iter()
            .filter(|(sub_dag, _)| leader_rounds.contains(&sub_dag.leader.round))
            .collect()
    }

    pub(crate) fn get_sub_dag_and_certified_commits(
        &mut self,
        leader_rounds: RangeInclusive<Round>,
    ) -> Vec<(CommittedSubDag, CertifiedCommit)> {
        let commits = self.get_sub_dag_and_commits(leader_rounds);
        commits
            .into_iter()
            .map(|(sub_dag, commit)| {
                let certified_commit =
                    CertifiedCommit::new_certified(commit, sub_dag.blocks.clone());
                (sub_dag, certified_commit)
            })
            .collect()
    }

    pub fn leader_blocks(&self, rounds: RangeInclusive<Round>) -> Vec<Option<VerifiedBlock>> {
        assert!(
            !self.blocks.is_empty(),
            "No blocks have been created, please make sure that you have called build method"
        );
        rounds
            .into_iter()
            .map(|round| self.leader_block(round))
            .collect()
    }

    pub(crate) fn leader_block(&self, round: Round) -> Option<VerifiedBlock> {
        assert!(
            !self.blocks.is_empty(),
            "No blocks have been created, please make sure that you have called build method"
        );
        self.blocks
            .iter()
            .find(|(block_ref, _block)| {
                block_ref.round == round
                    && block_ref.author == self.leader_schedule.elect_leader(round, 0)
            })
            .map(|(_block_ref, block)| block.clone())
    }

    pub(crate) fn layer(&mut self, round: Round) -> LayerBuilder {
        LayerBuilder::new(self, round)
    }

    pub fn layers(&mut self, rounds: RangeInclusive<Round>) -> LayerBuilder {
        let mut builder = LayerBuilder::new(self, *rounds.start());
        builder.end_round = Some(*rounds.end());
        builder
    }

    pub(crate) fn persist_all_blocks(&self, dag_state: Arc<RwLock<DagState>>) {
        dag_state
            .write()
            .accept_blocks(self.blocks.values().cloned().collect());
    }

    pub(crate) fn print(&self) {
        let mut dag_str = "DAG {\n".to_string();

        let mut round = 0;
        for block in self.blocks.values() {
            if block.round() > round {
                round = block.round();
                dag_str.push_str(&format!("Round {round} : \n"));
            }
            dag_str.push_str(&format!("    Block {block:#?}\n"));
        }
        dag_str.push_str("}\n");

        tracing::info!("{dag_str}");
    }

    // TODO: merge into layer builder?
    // This method allows the user to specify specific links to ancestors. The
    // layer is written to dag state and the blocks are cached in [`DagBuilder`]
    // state.
    pub(crate) fn layer_with_connections(
        &mut self,
        connections: Vec<(AuthorityIndex, Vec<BlockRef>)>,
        round: Round,
    ) {
        let mut references = Vec::new();
        for (authority, ancestors) in connections {
            let author = authority.value() as u32;
            let base_ts = round as BlockTimestampMs * 1000;
            let block = VerifiedBlock::new_for_test(
                TestBlock::new(round, author)
                    .set_ancestors(ancestors)
                    .set_timestamp_ms(base_ts + author as u64)
                    .build(),
            );
            references.push(block.reference());
            self.blocks.insert(block.reference(), block.clone());
        }
        self.last_ancestors = references;
    }

    /// Gets all uncommitted blocks in a slot.
    pub(crate) fn get_uncommitted_blocks_at_slot(&self, slot: Slot) -> Vec<VerifiedBlock> {
        let mut blocks = vec![];
        for (_block_ref, block) in self.blocks.range((
            Included(BlockRef::new(slot.round, slot.authority, BlockDigest::MIN)),
            Included(BlockRef::new(slot.round, slot.authority, BlockDigest::MAX)),
        )) {
            blocks.push(block.clone())
        }
        blocks
    }

    pub(crate) fn genesis_block_refs(&self) -> Vec<BlockRef> {
        self.genesis.keys().cloned().collect()
    }
}

/// Refer to doc comments for [`DagBuilder`] for usage information.
pub struct LayerBuilder<'a> {
    dag_builder: &'a mut DagBuilder,

    start_round: Round,
    end_round: Option<Round>,

    // Configuration options applied to specified authorities
    // TODO: convert configuration options into an enum
    specified_authorities: Option<Vec<AuthorityIndex>>,
    // Number of transactions to include per block.
    num_transactions: u32,
    // Whether to include rejected transactions
    rejected_transactions_pct: u8,
    rejected_transactions_seed: u64,
    // Number of equivocating blocks per specified authority
    equivocations: usize,
    // Skip block proposal for specified authorities
    skip_block: bool,
    // Skip specified ancestor links for specified authorities
    skip_ancestor_links: Option<Vec<AuthorityIndex>>,
    // Skip leader link for specified authorities
    no_leader_link: bool,
    // Use to override last ancestors in dag builder
    override_last_ancestors: bool,

    // Skip leader block proposal
    no_leader_block: bool,
    // Used for leader based configurations
    specified_leader_link_offsets: Option<Vec<u32>>,
    specified_leader_block_offsets: Option<Vec<u32>>,
    leader_round: Option<Round>,

    // All ancestors will be linked to the current layer
    fully_linked_ancestors: bool,
    // Only 2f+1 random ancestors will be linked to the current layer using a
    // seed, if provided
    min_ancestor_links: bool,
    min_ancestor_links_random_seed: Option<u64>,
    // Add random weak links to the current layer using a seed, if provided
    random_weak_links: bool,
    random_weak_links_random_seed: Option<u64>,

    // Ancestors to link to the current layer
    ancestors: Vec<BlockRef>,
    // override last ancestors in dag_builder
    specified_ancestors: Vec<BlockRef>,

    // The block timestamps for the layer for each specified authority. This will work as base timestamp and the round will be added to make sure that timestamps do offset.
    timestamps: Vec<BlockTimestampMs>,

    // Accumulated blocks to write to dag state
    blocks: Vec<VerifiedBlock>,
}

#[allow(unused)]
impl<'a> LayerBuilder<'a> {
    fn new(dag_builder: &'a mut DagBuilder, start_round: Round) -> Self {
        assert!(start_round > 0, "genesis round is created by default");
        let ancestors = dag_builder.last_ancestors.clone();
        Self {
            dag_builder,
            start_round,
            end_round: None,
            specified_authorities: None,
            num_transactions: 0,
            rejected_transactions_pct: 0,
            rejected_transactions_seed: 0,
            equivocations: 0,
            skip_block: false,
            skip_ancestor_links: None,
            override_last_ancestors: false,
            no_leader_link: false,
            no_leader_block: false,
            specified_leader_link_offsets: None,
            specified_leader_block_offsets: None,
            leader_round: None,
            fully_linked_ancestors: true,
            min_ancestor_links: false,
            min_ancestor_links_random_seed: None,
            random_weak_links: false,
            random_weak_links_random_seed: None,
            ancestors,
            specified_ancestors: vec![],
            timestamps: vec![],
            blocks: vec![],
        }
    }

    // Configuration methods

    // If you try to add blocks to the same layer you will end up with the last
    // ancestors replaced with blocks from the current layer. Use this method
    // to override the last ancestors with the references you manually have kept.
    pub fn override_last_ancestors(mut self, ancestors: Vec<BlockRef>) -> Self {
        self.specified_ancestors = ancestors;
        self.override_last_ancestors = true;
        self.build()
    }

    // Only link 2f+1 random ancestors to the current layer round using a seed,
    // if provided. Also provide a flag to guarantee the leader is included.
    // note: configuration is terminal and layer will be built after this call.
    pub fn min_ancestor_links(mut self, include_leader: bool, seed: Option<u64>) -> Self {
        self.min_ancestor_links = true;
        self.min_ancestor_links_random_seed = seed;
        if include_leader {
            self.leader_round = Some(self.ancestors.iter().max_by_key(|b| b.round).unwrap().round);
        }
        self.fully_linked_ancestors = false;
        self.build()
    }

    // No links will be created between the specified ancestors and the specified
    // authorities at the layer round.
    // note: configuration is terminal and layer will be built after this call.
    pub fn skip_ancestor_links(mut self, ancestors_to_skip: Vec<AuthorityIndex>) -> Self {
        // authorities must be specified for this to apply
        assert!(self.specified_authorities.is_some());
        self.skip_ancestor_links = Some(ancestors_to_skip);
        self.fully_linked_ancestors = false;
        self.build()
    }

    // Add random weak links to the current layer round using a seed, if provided
    pub fn random_weak_links(mut self, seed: Option<u64>) -> Self {
        self.random_weak_links = true;
        self.random_weak_links_random_seed = seed;
        self
    }

    // Should be called when building a leader round. Will ensure leader block is missing.
    // A list of specified leader offsets can be provided to skip those leaders.
    // If none are provided all potential leaders for the round will be skipped.
    pub fn no_leader_block(mut self, specified_leader_offsets: Vec<u32>) -> Self {
        self.no_leader_block = true;
        self.specified_leader_block_offsets = Some(specified_leader_offsets);
        self
    }

    // Should be called when building a voting round. Will ensure vote is missing.
    // A list of specified leader offsets can be provided to skip those leader links.
    // If none are provided all potential leaders for the round will be skipped.
    // note: configuration is terminal and layer will be built after this call.
    pub fn no_leader_link(
        mut self,
        leader_round: Round,
        specified_leader_offsets: Vec<u32>,
    ) -> Self {
        self.no_leader_link = true;
        self.specified_leader_link_offsets = Some(specified_leader_offsets);
        self.leader_round = Some(leader_round);
        self.fully_linked_ancestors = false;
        self.build()
    }

    pub fn authorities(mut self, authorities: Vec<AuthorityIndex>) -> Self {
        assert!(
            self.specified_authorities.is_none(),
            "Specified authorities already set"
        );
        self.specified_authorities = Some(authorities);
        self
    }

    // Number of transactions to include per block.
    pub fn num_transactions(mut self, num_transactions: u32) -> Self {
        self.num_transactions = num_transactions;
        self
    }

    pub fn rejected_transactions_pct(mut self, pct: u8, seed: Option<u64>) -> Self {
        self.rejected_transactions_pct = pct;
        self.rejected_transactions_seed = if let Some(seed) = seed {
            seed
        } else {
            thread_rng().gen()
        };
        self
    }

    // Multiple blocks will be created for the specified authorities at the layer round.
    pub fn equivocate(mut self, equivocations: usize) -> Self {
        // authorities must be specified for this to apply
        assert!(self.specified_authorities.is_some());
        self.equivocations = equivocations;
        self
    }

    // No blocks will be created for the specified authorities at the layer round.
    pub fn skip_block(mut self) -> Self {
        // authorities must be specified for this to apply
        assert!(self.specified_authorities.is_some());
        self.skip_block = true;
        self
    }

    pub fn with_timestamps(mut self, timestamps: Vec<BlockTimestampMs>) -> Self {
        // authorities must be specified for this to apply
        assert!(self.specified_authorities.is_some());
        assert_eq!(
            self.specified_authorities.as_ref().unwrap().len(),
            timestamps.len(),
            "Timestamps should be provided for each specified authority"
        );
        self.timestamps = timestamps;
        self
    }

    // Apply the configurations & build the dag layer(s).
    pub fn build(mut self) -> Self {
        for round in self.start_round..=self.end_round.unwrap_or(self.start_round) {
            tracing::debug!("BUILDING LAYER ROUND {round}...");

            let authorities = if self.specified_authorities.is_some() {
                self.specified_authorities.clone().unwrap()
            } else {
                self.dag_builder
                    .context
                    .committee
                    .authorities()
                    .map(|x| x.0)
                    .collect()
            };

            // TODO: investigate if these configurations can be called in combination
            // for the same layer
            let mut connections = if self.override_last_ancestors {
                self.configure_specifed_ancestors()
            } else if self.fully_linked_ancestors {
                self.configure_fully_linked_ancestors(round)
            } else if self.min_ancestor_links {
                self.configure_min_parent_links(round)
            } else if self.no_leader_link {
                self.configure_no_leader_links(authorities.clone(), round)
            } else if self.skip_ancestor_links.is_some() {
                self.configure_skipped_ancestor_links(
                    authorities,
                    self.skip_ancestor_links.clone().unwrap(),
                )
            } else {
                vec![]
            };

            if self.random_weak_links {
                connections.append(&mut self.configure_random_weak_links());
            }

            self.create_blocks(round, connections);
        }

        self.dag_builder.last_ancestors = self.ancestors.clone();
        self
    }

    pub fn persist_layers(&self, dag_state: Arc<RwLock<DagState>>) {
        assert!(!self.blocks.is_empty(), "Called to persist layers although no blocks have been created. Make sure you have called build before.");
        dag_state.write().accept_blocks(self.blocks.clone());
    }

    // Layer round is minimally and randomly connected with ancestors.
    pub fn configure_min_parent_links(
        &mut self,
        round: Round,
    ) -> Vec<(AuthorityIndex, Vec<BlockRef>)> {
        let quorum_threshold = self.dag_builder.context.committee.quorum_threshold() as usize;
        let mut authorities: Vec<AuthorityIndex> = self
            .dag_builder
            .context
            .committee
            .authorities()
            .map(|authority| authority.0)
            .collect();

        let mut rng = match self.min_ancestor_links_random_seed {
            Some(s) => StdRng::seed_from_u64(s),
            None => StdRng::from_entropy(),
        };

        let mut authorities_to_shuffle = authorities.clone();

        let mut leaders = vec![];
        if let Some(leader_round) = self.leader_round {
            let leader_offsets = (0..self.dag_builder.number_of_leaders).collect::<Vec<_>>();

            for leader_offset in leader_offsets {
                leaders.push(
                    self.dag_builder
                        .leader_schedule
                        .elect_leader(leader_round, leader_offset),
                );
            }
        }

        authorities
            .iter()
            .map(|authority| {
                authorities_to_shuffle.shuffle(&mut rng);

                // TODO: handle quroum threshold properly with stake
                let min_ancestors: HashSet<AuthorityIndex> = authorities_to_shuffle
                    .iter()
                    .take(quorum_threshold)
                    .cloned()
                    .collect();

                (
                    *authority,
                    self.ancestors
                        .iter()
                        .filter(|a| {
                            leaders.contains(&a.author)
                                || min_ancestors.contains(&a.author)
                                || a.round != round
                        })
                        .cloned()
                        .collect::<Vec<BlockRef>>(),
                )
            })
            .collect()
    }

    // TODO: configure layer round randomly connected with weak links.
    fn configure_random_weak_links(&mut self) -> Vec<(AuthorityIndex, Vec<BlockRef>)> {
        unimplemented!("configure_random_weak_links");
    }

    // Layer round misses link to leader, but other blocks are fully connected with ancestors.
    fn configure_no_leader_links(
        &mut self,
        authorities: Vec<AuthorityIndex>,
        round: Round,
    ) -> Vec<(AuthorityIndex, Vec<BlockRef>)> {
        let mut missing_leaders = Vec::new();
        let mut specified_leader_offsets = self
            .specified_leader_link_offsets
            .clone()
            .expect("specified_leader_offsets should be set");
        let leader_round = self.leader_round.expect("leader round should be set");

        // When no specified leader offsets are available, all leaders are
        // expected to be missing.
        if specified_leader_offsets.is_empty() {
            specified_leader_offsets.extend(0..self.dag_builder.number_of_leaders);
        }

        for leader_offset in specified_leader_offsets {
            missing_leaders.push(
                self.dag_builder
                    .leader_schedule
                    .elect_leader(leader_round, leader_offset),
            );
        }

        self.configure_skipped_ancestor_links(authorities, missing_leaders)
    }

    fn configure_specifed_ancestors(&mut self) -> Vec<(AuthorityIndex, Vec<BlockRef>)> {
        self.dag_builder
            .context
            .committee
            .authorities()
            .map(|authority| (authority.0, self.specified_ancestors.clone()))
            .collect::<Vec<_>>()
    }

    fn configure_fully_linked_ancestors(
        &mut self,
        round: Round,
    ) -> Vec<(AuthorityIndex, Vec<BlockRef>)> {
        self.dag_builder
            .context
            .committee
            .authorities()
            .map(|authority| {
                (
                    authority.0,
                    // don't connect to ancestors of this round
                    self.ancestors
                        .clone()
                        .into_iter()
                        .filter(|a| a.round != round)
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
    }

    fn configure_skipped_ancestor_links(
        &mut self,
        authorities: Vec<AuthorityIndex>,
        ancestors_to_skip: Vec<AuthorityIndex>,
    ) -> Vec<(AuthorityIndex, Vec<BlockRef>)> {
        let filtered_ancestors = self
            .ancestors
            .clone()
            .into_iter()
            .filter(|ancestor| !ancestors_to_skip.contains(&ancestor.author))
            .collect::<Vec<_>>();
        authorities
            .into_iter()
            .map(|authority| (authority, filtered_ancestors.clone()))
            .collect::<Vec<_>>()
    }

    // Creates the blocks for the new layer based on configured connections, also
    // sets the ancestors for future layers to be linked to
    fn create_blocks(&mut self, round: Round, connections: Vec<(AuthorityIndex, Vec<BlockRef>)>) {
        let mut references = Vec::new();
        let mut reject_rng =
            StdRng::seed_from_u64(self.rejected_transactions_seed ^ (round as u64));
        for (authority, ancestors) in connections {
            if self.should_skip_block(round, authority) {
                continue;
            };
            let transactions = (0..self.num_transactions)
                .map(|_| Transaction::new(vec![1_u8; 16]))
                .collect::<Vec<_>>();
            let num_blocks = self.num_blocks_to_create(authority);
            for num_block in 0..num_blocks {
                let mut votes = vec![];
                if self.rejected_transactions_pct > 0 {
                    for ancestor in &ancestors {
                        let mut rejects = vec![];
                        for i in 0..self.num_transactions {
                            if reject_rng.gen_range(1..=100) <= self.rejected_transactions_pct {
                                rejects.push(i as TransactionIndex);
                            }
                        }
                        if !rejects.is_empty() {
                            votes.push(BlockTransactionVotes {
                                block_ref: *ancestor,
                                rejects,
                            });
                        }
                    }
                }
                let timestamp = self.block_timestamp(authority, round, num_block);
                let block = VerifiedBlock::new_for_test(
                    TestBlock::new(round, authority.value() as u32)
                        .set_transactions(transactions.clone())
                        .set_transaction_votes(votes)
                        .set_ancestors(ancestors.clone())
                        .set_timestamp_ms(timestamp)
                        .build(),
                );
                references.push(block.reference());
                self.dag_builder
                    .blocks
                    .insert(block.reference(), block.clone());
                self.blocks.push(block);
            }
        }
        self.ancestors = references;
    }

    fn num_blocks_to_create(&self, authority: AuthorityIndex) -> u32 {
        if self.specified_authorities.is_some()
            && self
                .specified_authorities
                .clone()
                .unwrap()
                .contains(&authority)
        {
            // Always create 1 block and then the equivocating blocks on top of that.
            1 + self.equivocations as u32
        } else {
            1
        }
    }

    fn block_timestamp(
        &self,
        authority: AuthorityIndex,
        round: Round,
        num_block: u32,
    ) -> BlockTimestampMs {
        if self.specified_authorities.is_some() && !self.timestamps.is_empty() {
            let specified_authorities = self.specified_authorities.as_ref().unwrap();

            if let Some(position) = specified_authorities.iter().position(|&x| x == authority) {
                return self.timestamps[position] + (round + num_block) as u64;
            }
        }
        let author = authority.value() as u32;
        let base_ts = round as BlockTimestampMs * 1000;
        base_ts + (author + round + num_block) as u64
    }

    fn should_skip_block(&self, round: Round, authority: AuthorityIndex) -> bool {
        // Safe to unwrap as specified authorites has to be set before skip
        // is specified.
        if self.skip_block
            && self
                .specified_authorities
                .clone()
                .unwrap()
                .contains(&authority)
        {
            return true;
        }
        if self.no_leader_block {
            let mut specified_leader_offsets = self
                .specified_leader_block_offsets
                .clone()
                .expect("specified_leader_block_offsets should be set");

            // When no specified leader offsets are available, all leaders are
            // expected to be skipped.
            if specified_leader_offsets.is_empty() {
                specified_leader_offsets.extend(0..self.dag_builder.number_of_leaders);
            }

            for leader_offset in specified_leader_offsets {
                let leader = self
                    .dag_builder
                    .leader_schedule
                    .elect_leader(round, leader_offset);

                if leader == authority {
                    return true;
                }
            }
        }
        false
    }
}

// TODO: add unit tests
