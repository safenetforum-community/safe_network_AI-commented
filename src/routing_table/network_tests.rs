// Copyright 2015 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement, version 1.1.  This, along with the
// Licenses can be found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

#![cfg(test)]

use maidsafe_utilities::SeededRng;
use rand::Rng;
use std::cmp;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::collections::hash_map::Entry;
use std::fmt::{self, Binary, Debug, Formatter};
use std::iter::{FromIterator, IntoIterator};
use super::{Destination, Error, OtherMergeDetails, RoutingTable};
use super::prefix::Prefix;
use super::xorable::Xorable;

const MIN_GROUP_SIZE: usize = 8;

#[derive(Clone, Eq, PartialEq)]
struct Contact(u64);

/// A simulated network, consisting of a set of "nodes" (routing tables) and a random number
/// generator.
#[derive(Default)]
struct Network {
    rng: SeededRng,
    nodes: HashMap<u64, RoutingTable<u64>>,
}

impl Network {
    /// Creates a new empty network with a seeded random number generator.
    fn new(optional_seed: Option<[u32; 4]>) -> Network {
        Network {
            rng: optional_seed.map_or_else(SeededRng::new, SeededRng::from_seed),
            nodes: HashMap::new(),
        }
    }

    /// Adds a new node to the network and makes it join its new group, splitting if necessary.
    fn add_node(&mut self) {
        let name = self.random_free_name(); // The new node's name.
        if self.nodes.is_empty() {
            // If this is the first node, just add it and return.
            assert!(self.nodes.insert(name, RoutingTable::new(name, MIN_GROUP_SIZE)).is_none());
            return;
        }

        let mut new_table = {
            let close_node = self.close_node(name);
            let close_peer = &self.nodes[&close_node];
            unwrap!(RoutingTable::new_with_prefixes(name, MIN_GROUP_SIZE, close_peer.prefixes()))
        };
        
        let mut split_prefixes = BTreeSet::new();
        // TODO: needs to verify how to broadcasting such info
        for node in self.nodes.values_mut() {
            match node.add(name) {
                Ok(true) => {
                    split_prefixes.insert(*node.our_group_prefix());
                }
                Ok(false) => {}
                Err(e) => trace!("failed to add node with error {:?}", e),
            }
            match new_table.add(*node.our_name()) {
                Ok(true) => {
                    let prefix = *new_table.our_group_prefix();
                    let _ = new_table.split(prefix);
                }
                Ok(false) => {}
                Err(e) => trace!("failed to add node into new with error {:?}", e),
            }
        }
        assert!(self.nodes.insert(name, new_table).is_none());
        for split_prefix in &split_prefixes {
            for node in self.nodes.values_mut() {
                let _ = node.split(*split_prefix);
            }
        }
    }

    /// Drops a node and, if necessary, merges groups to restore the group requirement.
    fn drop_node(&mut self) {
        let keys = self.keys();
        let name = *unwrap!(self.rng.choose(&keys));
        let _ = self.nodes.remove(&name);
        let mut merge_own_info = Vec::new();
        // TODO: needs to verify how to broadcasting such info
        for node in self.nodes.values_mut() {
            if node.iter().any(|&name_in_table| name_in_table == name) {
                let removed_node_is_in_our_group = node.is_in_our_group(&name);
                let removal_details = unwrap!(node.remove(&name));
                assert_eq!(name, removal_details.name);
                assert_eq!(removed_node_is_in_our_group,
                           removal_details.was_in_our_group);
                match removal_details.targets_and_merge_details {
                    // TODO: shall a panic be raised in case of failure?
                    None => {}
                    Some(info) => {
                        merge_own_info.push(info);
                    }
                }
            } else {
                match node.remove(&name) {
                    Err(Error::NoSuchPeer) => {}
                    Err(error) => panic!("Expected NoSuchPeer, but got {:?}", error),
                    Ok(details) => panic!("Expected NoSuchPeer, but got {:?}", details),
                }
            }
        }

        let mut merge_other_info = Vec::new();

        // handle broadcast of merge_own_group
        for (targets, merge_own_details) in merge_own_info {
            for target in targets {
                let target_node = unwrap!(self.nodes.get_mut(&target));
                let other_info = target_node.merge_own_group(&merge_own_details);
                merge_other_info.push(other_info);
                // add needed contacts
                let needed = target_node.needed().clone();
                for needed_contact in needed {
                    target_node.add(needed_contact);
                }
            }
        }

        // handle broadcast of merge_other_group
        for (targets, merge_other_details) in merge_other_info {
            for target in targets {
                let target_node = unwrap!(self.nodes.get_mut(&target));
                let contacts = target_node.merge_other_group(&merge_other_details);
                // add missing contacts
                for contact in contacts {
                    target_node.add(contact);
                }
            }
        }
    }

    /// Returns a random name that is not taken by any node yet.
    fn random_free_name(&mut self) -> u64 {
        loop {
            let name = self.rng.gen();
            if !self.nodes.contains_key(&name) {
                return name;
            }
        }
    }

    /// Verifies that a message sent from node `src` would arrive at destination `dst` via the
    /// given `route`.
    fn send_message(&self, src: u64, dst: Destination<u64>, route: usize) {
        let mut received = Vec::new(); // These nodes have received but not handled the message.
        let mut handled = HashSet::new(); // These nodes have received and handled the message.
        received.push(src);
        while let Some(node) = received.pop() {
            handled.insert(node); // `node` is now handling the message and relaying it.
            if Destination::Node(node) != dst {
                for target in unwrap!(self.nodes[&node].targets(&dst, route)) {
                    if !handled.contains(&target) && !received.contains(&target) {
                        received.push(target);
                    }
                }
            }
        }
        match dst {
            Destination::Node(node) => assert!(handled.contains(&node)),
            Destination::Group(address) => {
                let close_node = self.close_node(address);
                for node in unwrap!(self.nodes[&close_node].close_names(&address)) {
                    assert!(handled.contains(&node));
                }
            }
        }
    }

    /// Returns any node that's close to the given address. Panics if the network is empty or no
    /// node is found.
    fn close_node(&self, address: u64) -> u64 {
        let target = Destination::Group(address);
        unwrap!(self.nodes
            .iter()
            .find(|&(_, table)| table.is_recipient(&target))
            .map(|(&peer, _)| peer))
    }

    /// Returns all node names.
    fn keys(&self) -> Vec<u64> {
        self.nodes.keys().cloned().collect()
    }
}

#[test]
fn node_to_node_message() {
    let mut network = Network::new(None);
    for _ in 0..100 {
        network.add_node();
    }
    let keys = network.keys();
    for _ in 0..20 {
        let src = *unwrap!(network.rng.choose(&keys));
        let dst = *unwrap!(network.rng.choose(&keys));
        for route in 0..MIN_GROUP_SIZE {
            network.send_message(src, Destination::Node(dst), route);
        }
    }
}

#[test]
fn node_to_group_message() {
    let mut network = Network::new(None);
    for _ in 0..100 {
        network.add_node();
    }
    let keys = network.keys();
    for _ in 0..20 {
        let src = *unwrap!(network.rng.choose(&keys));
        let dst = network.rng.gen();
        for route in 0..MIN_GROUP_SIZE {
            network.send_message(src, Destination::Group(dst), route);
        }
    }
}

fn verify_invariant(network: &Network) {
    let mut groups: HashMap<Prefix<u64>, HashSet<u64>> = HashMap::new();
    // first, collect all groups in the network
    for node in network.nodes.values() {
        for prefix in node.groups.keys() {
            let mut group_content = node.groups[prefix].clone();
            if *prefix == node.our_group_prefix {
                group_content.insert(*node.our_name());
            }
            if let Some(group) = groups.get_mut(prefix) {
                group.extend(group_content);
                continue;
            }
            let _ = groups.insert(*prefix, group_content);
        }
        // use this opportunity to check if each node satisfies the invariant
        node.verify_invariant();
    }
    // check that prefixes are disjoint
    verify_disjoint_prefixes(groups.keys().into_iter().cloned().collect());

    // check that each group contains names agreeing with its prefix
    verify_groups_match_names(&groups);

    // check that groups cover the whole namespace
    assert_eq!(network.nodes.len(), groups.values().map(|x| x.len()).sum());
}

fn verify_disjoint_prefixes(prefixes: HashSet<Prefix<u64>>) {
    for prefix in &prefixes {
        assert!(!prefixes.iter()
            .any(|x| *x != *prefix && (x.is_compatible(prefix) || prefix.is_compatible(x))));
    }
}

fn verify_groups_match_names(groups: &HashMap<Prefix<u64>, HashSet<u64>>) {
    for &prefix in groups.keys() {
        for name in &groups[&prefix] {
            assert!(prefix.matches(name));
        }
    }
}

#[test]
fn groups_have_identical_routing_tables() {
    let mut network = Network::new(None);
    for _ in 0..100 {
        network.add_node();
    }
    verify_invariant(&network);
}

#[test]
fn merging_groups() {
    let mut network = Network::new(None);
    for i in 0..100 {
        network.add_node();
        verify_invariant(&network);
    }
    assert!(network.nodes
        .iter()
        .all(|(_, table)| if table.num_of_groups() < 3 {
            trace!("{:?}", table);
            false
        } else {
            true
        }));
    for i in 0..95 {
        network.drop_node();
        verify_invariant(&network);
    }
    assert!(network.nodes
        .iter()
        .all(|(_, table)| if table.num_of_groups() > 1 {
            trace!("{:?}", table);
            false
        } else {
            true
        }));
}
