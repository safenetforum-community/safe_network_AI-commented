// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use crate::node::{
    flow_ctrl::cmds::Cmd,
    membership::{self, Membership},
    messaging::Recipients,
    MyNode, NodeContext, Result,
};

use bls::Signature;
use sn_consensus::{Decision, Generation, SignedVote, VoteResponse};
use sn_interface::{
    messaging::system::{JoinResponse, NodeMsg, SectionSig, SectionSigned},
    network_knowledge::{node_state::RelocationTrigger, MembershipState, NodeState},
    types::{log_markers::LogMarker, NodeId, Participant},
};

use std::{collections::BTreeSet, vec};

// Message handling
impl MyNode {
    pub(crate) fn propose_membership_change(&mut self, node_state: NodeState) -> Option<Cmd> {
        info!(
            "Proposing membership change: {} - {:?}",
            node_state.name(),
            node_state.state()
        );

        let context = &self.context();
        let prefix = self.network_knowledge.prefix();
        if let Some(membership) = self.membership.as_mut() {
            let membership_vote = match membership.propose(node_state, &prefix) {
                Ok(vote) => vote,
                Err(e) => {
                    warn!("Membership - failed to propose change: {e:?}");
                    return None;
                }
            };
            Some(MyNode::send_to_elders(
                context,
                NodeMsg::MembershipVotes(vec![membership_vote]),
            ))
        } else {
            error!("Membership - Failed to propose membership change, no membership instance");
            None
        }
    }

    /// Get our latest vote if any at this generation, and get cmds to resend to all elders
    /// (which should in turn trigger them to resend their votes)
    #[instrument(skip_all)]
    pub(crate) fn membership_gossip_votes(context: &NodeContext) -> Option<Cmd> {
        if let Some(membership) = &context.membership {
            trace!("{}", LogMarker::GossippingMembershipVotes);
            if let Ok(ae_votes) = membership.anti_entropy(membership.generation()) {
                let cmd = MyNode::send_to_elders(context, NodeMsg::MembershipVotes(ae_votes));
                return Some(cmd);
            }
        }

        None
    }

    pub(crate) fn handle_membership_votes(
        &mut self,
        node_id: NodeId,
        signed_votes: Vec<SignedVote<NodeState>>,
    ) -> Result<Vec<Cmd>> {
        trace!(
            "{:?} {signed_votes:?} from {node_id}",
            LogMarker::MembershipVotesBeingHandled
        );

        let context = &self.context();
        let prefix = context.network_knowledge.prefix();

        let mut cmds = vec![];

        for signed_vote in signed_votes {
            let mut vote_broadcast = None;
            if let Some(membership) = self.membership.as_mut() {
                let (vote_response, decision) = match membership
                    .handle_signed_vote(signed_vote, &prefix)
                {
                    Ok(result) => result,
                    Err(membership::Error::RequestAntiEntropy) => {
                        debug!("Membership - We are behind the voter, requesting AE");
                        // We hit an error while processing this vote, perhaps we are missing information.
                        // We'll send a membership AE request to see if they can help us catch up.
                        debug!("{:?}", LogMarker::MembershipSendingAeUpdateRequest);
                        let msg = NodeMsg::MembershipAE(membership.generation());
                        cmds.push(Cmd::send_msg(
                            msg,
                            Recipients::Single(Participant::from_node(node_id)),
                        ));
                        // return the vec w/ the AE cmd there so as not to loop and generate AE for
                        // any subsequent commands
                        return Ok(cmds);
                    }
                    Err(e) => {
                        error!("Membership - error while processing vote {e:?}, dropping this and all votes in this batch thereafter");
                        break;
                    }
                };

                match vote_response {
                    VoteResponse::Broadcast(response_vote) => {
                        vote_broadcast = Some(NodeMsg::MembershipVotes(vec![response_vote]));
                    }
                    VoteResponse::WaitingForMoreVotes => {
                        // do nothing
                    }
                };

                if let Some(decision) = decision {
                    cmds.push(Cmd::HandleMembershipDecision(decision));
                }
            } else {
                error!(
                    "Attempted to handle membership vote when we don't yet have a membership instance"
                );
            };

            if let Some(vote_msg) = vote_broadcast {
                cmds.push(MyNode::send_to_elders(context, vote_msg));
            }
        }

        Ok(cmds)
    }

    pub(crate) fn handle_membership_anti_entropy_request(
        membership_context: &Option<Membership>,
        node_id: NodeId,
        gen: Generation,
    ) -> Option<Cmd> {
        debug!(
            "{:?} membership anti-entropy request for gen {gen:?} from {node_id}",
            LogMarker::MembershipAeRequestReceived,
        );

        if let Some(membership) = membership_context {
            match membership.anti_entropy(gen) {
                Ok(catchup_votes) => {
                    trace!("Sending catchup votes to {node_id:?}");
                    Some(Cmd::send_msg(
                        NodeMsg::MembershipVotes(catchup_votes),
                        Recipients::Single(Participant::from_node(node_id)),
                    ))
                }
                Err(e) => {
                    error!("Membership - Error while processing anti-entropy {:?}", e);
                    None
                }
            }
        } else {
            error!(
                "Attempted to handle membership anti-entropy when we don't yet have a membership instance"
            );
            None
        }
    }

    pub(crate) async fn handle_membership_decision(
        &mut self,
        decision: Decision<NodeState>,
    ) -> Result<Vec<Cmd>> {
        info!("{}", LogMarker::AgreementOfMembership);
        let mut cmds = vec![];

        let (joining_nodes, leaving_nodes): (Vec<_>, Vec<_>) = decision
            .proposals
            .clone()
            .into_iter()
            .partition(|(n, _)| n.state() == MembershipState::Joined);

        info!(
            "Handling membership decision: joining = {:?}, leaving = {:?}",
            Vec::from_iter(joining_nodes.iter().map(|(n, _)| n.name())),
            Vec::from_iter(leaving_nodes.iter().map(|(n, _)| n.name()))
        );

        for (new_info, signature) in joining_nodes.iter().cloned() {
            cmds.extend(self.handle_node_joined(new_info, signature).await);
        }

        for (new_info, signature) in leaving_nodes.iter().cloned() {
            cmds.extend(self.handle_node_left(new_info, signature).into_iter());
        }

        if !joining_nodes.is_empty() {
            cmds.push(self.send_node_approvals(decision.clone()));
        }

        // Do not disable node joins in first section.
        if !self.is_startup_joining_allowed() {
            // ..otherwise, switch off joins_allowed on a node joining.
            // TODO: fix racing issues here? https://github.com/maidsafe/safe_network/issues/890
            self.joins_allowed = false;
        }

        if !decision.proposals.is_empty() {
            let relocation_trigger = RelocationTrigger::new(decision);
            let excluded_from_relocation =
                BTreeSet::from_iter(joining_nodes.iter().map(|(n, _)| n.name()));

            cmds.extend(self.try_relocate_nodes(relocation_trigger, excluded_from_relocation)?);
        }

        cmds.extend(self.trigger_dkg()?);

        cmds.extend(self.send_ae_update_to_our_section()?);

        self.fault_detection_retain_only(
            self.network_knowledge
                .adults()
                .iter()
                .map(|node_id| node_id.name())
                .collect(),
            self.network_knowledge
                .elders()
                .iter()
                .map(|node_id| node_id.name())
                .collect(),
        )
        .await;

        if !leaving_nodes.is_empty() {
            self.joins_allowed = true;
        }

        let net_increase = joining_nodes.len() > leaving_nodes.len();

        // We do this check on every net node join.
        // It is a cheap check and any actual cleanup won't happen back to back,
        // due to requirement of `has_reached_min_capacity() == true` before doing it.
        if net_increase {
            self.data_storage
                .try_retain_data_of(self.network_knowledge.prefix());
            // if we are _still_ at min capacity, then it's time to allow joins until split
            if self.data_storage.has_reached_min_capacity() {
                self.joins_allowed = true;
                self.joins_allowed_until_split = true;
            }
        }

        // Once we've grown the section, we do not need to allow more nodes in.
        // (Unless we've triggered the storage critical fail safe to grow until split.)
        if net_increase && !self.is_startup_joining_allowed() && !self.joins_allowed_until_split {
            self.joins_allowed = false;
        }

        self.log_section_stats();
        self.log_network_stats();

        MyNode::update_comm_target_list(
            &self.comm,
            &self.network_knowledge.archived_members(),
            self.network_knowledge().members(),
        );

        // lets check that we have the correct data now we're changing membership
        cmds.push(MyNode::ask_for_any_new_data_from_whole_section(&self.context()).await);

        Ok(cmds)
    }

    pub(crate) fn is_startup_joining_allowed(&self) -> bool {
        const TEMP_SECTION_LIMIT: usize = 20;

        let is_first_section = self.network_knowledge.prefix().is_empty();
        let members_count = self.network_knowledge.members().len();

        if cfg!(feature = "limit-network-size") {
            is_first_section && members_count <= TEMP_SECTION_LIMIT
        } else {
            is_first_section
        }
    }

    async fn handle_node_joined(&mut self, new_state: NodeState, signature: Signature) -> Vec<Cmd> {
        let sig = SectionSig {
            public_key: self.network_knowledge.section_key(),
            signature,
        };

        let new_state = SectionSigned {
            value: new_state,
            sig,
        };

        if !self.network_knowledge.update_member(new_state.clone()) {
            info!("ignore Online: {}", new_state.node_id());
            return vec![];
        }

        self.add_new_adult_to_trackers(new_state.name()).await;

        info!("handle Online: {:?}", new_state.value);

        vec![]
    }

    // Send `NodeApproval` to a joining node which makes it a section member
    pub(crate) fn send_node_approvals(&self, decision: Decision<NodeState>) -> Cmd {
        let nodes: BTreeSet<_> = decision
            .proposals
            .keys()
            .filter(|n| n.state() == MembershipState::Joined)
            .map(|n| *n.node_id())
            .collect();
        let prefix = self.network_knowledge.prefix();
        info!("Section {prefix:?} has approved new nodes {nodes:?}.");

        let msg = NodeMsg::JoinResponse(JoinResponse::Approved(decision));

        trace!("{}", LogMarker::SendNodeApproval);
        Cmd::send_msg(msg, Recipients::Multiple(nodes))
    }

    pub(crate) fn handle_node_left(
        &mut self,
        node_state: NodeState,
        signature: Signature,
    ) -> Option<Cmd> {
        let sig = SectionSig {
            public_key: self.network_knowledge.section_key(),
            signature,
        };

        let node_state = SectionSigned {
            value: node_state,
            sig,
        };

        let _ = self.network_knowledge.update_member(node_state.clone());

        info!(
            "{}: {}",
            LogMarker::AcceptedNodeAsOffline,
            node_state.node_id()
        );

        // If this is an Offline agreement where the new node state is Relocated,
        // we then need to send the Relocate msg to the node attaching the signed NodeState
        // containing the relocation details.
        if node_state.is_relocated() {
            let node_id = *node_state.node_id();
            info!("Notify relocation to node {node_id:?}");
            let msg = NodeMsg::CompleteRelocation(node_state);
            Some(Cmd::send_msg(
                msg,
                Recipients::Single(Participant::from_node(node_id)),
            ))
        } else {
            None
        }
    }
}
