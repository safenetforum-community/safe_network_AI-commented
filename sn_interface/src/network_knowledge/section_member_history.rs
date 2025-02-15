// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use crate::network_knowledge::{errors::Result, MembershipState, NodeState, SectionsDAG};
use sn_consensus::Decision;
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use xor_name::{Prefix, XorName};

// Number of Elder churn events before a Left/Relocated member
// can be removed from the section members archive.
#[cfg(not(test))]
const ELDER_CHURN_EVENTS_TO_PRUNE_ARCHIVE: usize = 5;
#[cfg(test)]
const ELDER_CHURN_EVENTS_TO_PRUNE_ARCHIVE: usize = 3;

/// Container for storing information about (current and archived) members of our section.
#[derive(Clone, Default, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(super) struct SectionMemberHistory {
    members: BTreeMap<XorName, Decision<NodeState>>,
    archive: BTreeMap<XorName, Decision<NodeState>>,
}

impl SectionMemberHistory {
    /// Returns set of current members, i.e. those with state == `Joined`.
    pub(super) fn members(&self) -> BTreeSet<NodeState> {
        let mut node_state_list = BTreeSet::new();
        for (name, decision) in self.members.iter() {
            if let Some(node_state) = decision
                .proposals
                .keys()
                .find(|state| state.name() == *name)
            {
                let _ = node_state_list.insert(node_state.clone());
            }
        }
        node_state_list
    }

    /// Returns set of current members, i.e. those with state == `Joined`.
    /// with the correspondent Decision info
    pub(super) fn section_members_with_decision(&self) -> BTreeSet<Decision<NodeState>> {
        // TODO: ensure Decision has no Joined and Left combined within one
        self.members.values().cloned().collect()
    }

    /// Returns set of archived members, i.e those that've left our section
    pub(super) fn archived_members(&self) -> BTreeSet<NodeState> {
        let mut node_state_list = BTreeSet::new();
        for (name, decision) in self.archive.iter() {
            if let Some(node_state) = decision
                .proposals
                .keys()
                .find(|state| state.name() == *name)
            {
                let _ = node_state_list.insert(node_state.clone());
            }
        }
        node_state_list
    }

    /// Get the `NodeState` for the member with the given name.
    pub(super) fn get(&self, name: &XorName) -> Option<NodeState> {
        self.members()
            .iter()
            .find(|node_state| node_state.name() == *name)
            .cloned()
    }

    /// Returns whether the given node is currently a member of our section.
    pub(super) fn is_member(&self, name: &XorName) -> bool {
        self.get(name).is_some()
    }

    /// Update a member of our section.
    /// Returns whether anything actually changed.
    /// To maintain commutativity, the only allowed transitions are:
    /// - Joined -> Left
    /// - Joined -> Relocated
    /// - Relocated <--> Left (should not happen, but needed for consistency)
    pub(super) fn update(&mut self, new_decision: Decision<NodeState>) -> bool {
        let mut updated = false;
        for new_state in new_decision.proposals.keys() {
            let node_name = new_state.name();

            updated |= match (self.members.entry(node_name), new_state.state()) {
                (Entry::Vacant(entry), MembershipState::Joined) => {
                    // unless it was already archived, insert it as current member
                    if self.archive.contains_key(&node_name) {
                        false
                    } else {
                        entry.insert(new_decision.clone());
                        true
                    }
                }
                (Entry::Vacant(_), MembershipState::Left | MembershipState::Relocated(_)) => {
                    // insert it in our archive regardless it was there with another state
                    let _prev = self.archive.insert(node_name, new_decision.clone());
                    true
                }
                (Entry::Occupied(_), MembershipState::Joined) => false,
                (Entry::Occupied(entry), MembershipState::Left | MembershipState::Relocated(_)) => {
                    //  remove it from our current members, and insert it into our archive
                    let _ = entry.remove();
                    let _ = self.archive.insert(node_name, new_decision.clone());
                    true
                }
            };
        }
        updated
    }

    /// Remove all members whose name does not match `prefix`.
    pub(super) fn retain(&mut self, prefix: &Prefix) {
        self.members.retain(|name, _| prefix.matches(name))
    }

    // Remove any member which Left, or was Relocated, more
    // than ELDER_CHURN_EVENTS_TO_PRUNE_ARCHIVE section keys ago from `last_key`
    pub(super) fn prune_members_archive(
        &mut self,
        proof_chain: &SectionsDAG,
        last_key: &bls::PublicKey,
    ) -> Result<()> {
        let mut latest_section_keys = proof_chain.get_ancestors(last_key)?;
        latest_section_keys.truncate(ELDER_CHURN_EVENTS_TO_PRUNE_ARCHIVE - 1);
        latest_section_keys.push(*last_key);
        self.archive.retain(|_, decision| {
            latest_section_keys.iter().any(|section_key| {
                for (node_state, sig) in decision.proposals.iter() {
                    if bincode::serialize(node_state)
                        .map(|bytes| section_key.verify(sig, bytes))
                        .unwrap_or(false)
                    {
                        return true;
                    }
                }
                false
            })
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{SectionMemberHistory, SectionsDAG};
    use crate::{
        messaging::system::SectionSigned,
        network_knowledge::{MembershipState, NodeState},
        test_utils::{assert_lists, create_relocation_trigger, gen_addr, TestKeys},
        types::NodeId,
    };
    use eyre::Result;
    use rand::thread_rng;
    use sn_consensus::Decision;
    use std::collections::BTreeMap;
    use xor_name::XorName;

    #[test]
    fn retain_archived_members_of_the_latest_sections_while_pruning() -> Result<()> {
        let _rng = thread_rng();
        let mut section_members = SectionMemberHistory::default();

        // adding node set 1
        let sk_1 = bls::SecretKeySet::random(0, &mut thread_rng()).secret_key();
        let nodes_1 = gen_random_signed_node_states(1, MembershipState::Left, &sk_1)?;
        nodes_1.iter().for_each(|node| {
            section_members.update(node.clone());
        });
        let mut proof_chain = SectionsDAG::new(sk_1.public_key());
        // 1 should be retained
        section_members.prune_members_archive(&proof_chain, &sk_1.public_key())?;
        assert_lists(section_members.archive.values(), &nodes_1);

        // adding node set 2 as MembershipState::Relocated
        let sk_set_2 = bls::SecretKeySet::random(0, &mut thread_rng());
        let sk_2 = sk_set_2.secret_key();
        let (trigger, _) = create_relocation_trigger(&sk_set_2, 4)?;

        let nodes_2 = gen_random_signed_node_states(1, MembershipState::Relocated(trigger), &sk_2)?;
        nodes_2.iter().for_each(|node| {
            section_members.update(node.clone());
        });
        let sig = TestKeys::sign(&sk_1, &sk_2.public_key())?;
        proof_chain.verify_and_insert(&sk_1.public_key(), sk_2.public_key(), sig)?;
        // 1 -> 2 should be retained
        section_members.prune_members_archive(&proof_chain, &sk_2.public_key())?;
        assert_lists(
            section_members.archive.values(),
            nodes_1.iter().chain(&nodes_2),
        );

        // adding node set 3
        let sk_3 = bls::SecretKeySet::random(0, &mut thread_rng()).secret_key();
        let nodes_3 = gen_random_signed_node_states(1, MembershipState::Left, &sk_3)?;
        nodes_3.iter().for_each(|node| {
            section_members.update(node.clone());
        });
        let sig = TestKeys::sign(&sk_2, &sk_3.public_key())?;
        proof_chain.verify_and_insert(&sk_2.public_key(), sk_3.public_key(), sig)?;
        // 1 -> 2 -> 3 should be retained
        section_members.prune_members_archive(&proof_chain, &sk_3.public_key())?;
        assert_lists(
            section_members.archive.values(),
            nodes_1.iter().chain(&nodes_2).chain(&nodes_3),
        );

        // adding node set 4
        let sk_4 = bls::SecretKeySet::random(0, &mut thread_rng()).secret_key();
        let nodes_4 = gen_random_signed_node_states(1, MembershipState::Left, &sk_4)?;
        nodes_4.iter().for_each(|node| {
            section_members.update(node.clone());
        });
        let sig = TestKeys::sign(&sk_3, &sk_4.public_key())?;
        proof_chain.verify_and_insert(&sk_3.public_key(), sk_4.public_key(), sig)?;
        //  2 -> 3 -> 4 should be retained
        section_members.prune_members_archive(&proof_chain, &sk_4.public_key())?;
        assert_lists(
            section_members.archive.values(),
            nodes_2.iter().chain(&nodes_3).chain(&nodes_4),
        );

        // adding node set 5 as a branch to 3
        // 1 -> 2 -> 3 -> 4
        //              |
        //              -> 5
        let sk_5 = bls::SecretKeySet::random(0, &mut thread_rng()).secret_key();
        let nodes_5 = gen_random_signed_node_states(1, MembershipState::Left, &sk_5)?;
        nodes_5.iter().for_each(|node| {
            section_members.update(node.clone());
        });
        let sig = TestKeys::sign(&sk_3, &sk_5.public_key())?;
        proof_chain.verify_and_insert(&sk_3.public_key(), sk_5.public_key(), sig)?;
        // 2 -> 3 -> 5 should be retained
        section_members.prune_members_archive(&proof_chain, &sk_5.public_key())?;
        assert_lists(
            section_members.archive.values(),
            nodes_2.iter().chain(&nodes_3).chain(&nodes_5),
        );

        Ok(())
    }

    #[test]
    fn archived_members_should_not_be_moved_to_members_list() -> Result<()> {
        let _rng = thread_rng();
        let mut section_members = SectionMemberHistory::default();
        let sk_set = bls::SecretKeySet::random(0, &mut thread_rng());
        let sk = sk_set.secret_key();
        let node_left = gen_random_signed_node_states(1, MembershipState::Left, &sk)?[0].clone();
        let (trigger, _) = create_relocation_trigger(&sk_set, 4)?;
        let node_relocated =
            gen_random_signed_node_states(1, MembershipState::Relocated(trigger), &sk)?[0].clone();

        assert!(section_members.update(node_left.clone()));
        assert!(section_members.update(node_relocated.clone()));

        let (node_left_state, _) = node_left
            .proposals
            .first_key_value()
            .unwrap_or_else(|| panic!("Proposal of Decision is empty"));
        let (node_relocated_state, _) = node_relocated
            .proposals
            .first_key_value()
            .unwrap_or_else(|| panic!("Proposal of Decision is empty"));

        let node_left_joins =
            TestKeys::get_section_signed(&sk, NodeState::joined(*node_left_state.node_id(), None))?;
        let node_left_joins = section_signed_to_decision(node_left_joins);

        let node_relocated_joins = TestKeys::get_section_signed(
            &sk,
            NodeState::joined(*node_relocated_state.node_id(), None),
        )?;
        let node_relocated_joins = section_signed_to_decision(node_relocated_joins);

        assert!(!section_members.update(node_left_joins));
        assert!(!section_members.update(node_relocated_joins));

        assert_lists(
            section_members.archive.values(),
            &[node_left, node_relocated],
        );
        assert!(section_members.members().is_empty());

        Ok(())
    }

    #[test]
    fn members_should_be_archived_if_they_leave_or_relocate() -> Result<()> {
        let _rng = thread_rng();
        let mut section_members = SectionMemberHistory::default();
        let sk_set = bls::SecretKeySet::random(0, &mut thread_rng());
        let sk = sk_set.secret_key();

        let node_1 = gen_random_signed_node_states(1, MembershipState::Joined, &sk)?[0].clone();
        let (trigger, _) = create_relocation_trigger(&sk_set, 4)?;
        let node_2 =
            gen_random_signed_node_states(1, MembershipState::Relocated(trigger), &sk)?[0].clone();
        assert!(section_members.update(node_1.clone()));
        assert!(section_members.update(node_2.clone()));

        let (node_state_1, _) = node_1
            .proposals
            .first_key_value()
            .unwrap_or_else(|| panic!("Proposal of Decision is empty"));
        let (node_state_2, _) = node_2
            .proposals
            .first_key_value()
            .unwrap_or_else(|| panic!("Proposal of Decision is empty"));

        let node_1 = NodeState::left(*node_state_1.node_id(), Some(node_state_1.name()));
        let node_1 = TestKeys::get_section_signed(&sk, node_1)?;
        let node_1 = section_signed_to_decision(node_1);
        let node_2 = NodeState::left(*node_state_2.node_id(), Some(node_state_2.name()));
        let node_2 = TestKeys::get_section_signed(&sk, node_2)?;
        let node_2 = section_signed_to_decision(node_2);
        assert!(section_members.update(node_1.clone()));
        assert!(section_members.update(node_2.clone()));

        assert!(section_members.members().is_empty());
        assert_lists(section_members.archive.values(), &[node_1, node_2]);

        Ok(())
    }

    // Test helpers
    // generate node states signed by a section's sk
    fn gen_random_signed_node_states(
        num_nodes: usize,
        membership_state: MembershipState,
        secret_key: &bls::SecretKey,
    ) -> Result<Vec<Decision<NodeState>>> {
        let mut rng = thread_rng();
        let mut decisions = Vec::new();
        for _ in 0..num_nodes {
            let addr = gen_addr();
            let name = XorName::random(&mut rng);
            let node_id = NodeId::new(name, addr);
            let node_state = match membership_state {
                MembershipState::Joined => NodeState::joined(node_id, None),
                MembershipState::Left => NodeState::left(node_id, None),
                MembershipState::Relocated(ref trigger) => {
                    NodeState::relocated(node_id, None, trigger.clone())
                }
            };
            let section_signed_node_state = TestKeys::get_section_signed(secret_key, node_state)?;
            decisions.push(section_signed_to_decision(section_signed_node_state));
        }
        Ok(decisions)
    }

    // TODO: remove this function
    // Convert SectionSigned to Decision
    fn section_signed_to_decision(section_signed: SectionSigned<NodeState>) -> Decision<NodeState> {
        let mut proposals = BTreeMap::new();
        let _ = proposals.insert(section_signed.value, section_signed.sig.signature);
        Decision {
            generation: 0,
            proposals,
        }
    }
}
