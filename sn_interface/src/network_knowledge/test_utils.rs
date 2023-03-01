use super::{node_state::RelocationTrigger, NodeState, SectionKeysProvider};
use crate::{
    network_knowledge::{section_keys::build_spent_proof_share, Error, MyNodeInfo, MIN_ADULT_AGE},
    types::{keys::ed25519, NodeId},
    SectionAuthorityProvider,
};
use eyre::{eyre, Context, ContextCompat, Result};
use itertools::Itertools;
use sn_consensus::{Ballot, Consensus, Decision, Proposition, Vote, VoteResponse};
use sn_dbc::{
    get_public_commitments_from_transaction, Commitment, Dbc, DbcTransaction, Owner, OwnerOnce,
    Token, TransactionBuilder,
};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    fmt,
    net::SocketAddr,
};
use xor_name::Prefix;

// Parse `Prefix` from string
pub fn prefix(s: &str) -> Prefix {
    s.parse().expect("Failed to parse prefix")
}

// Generate unique SocketAddr for testing purposes
pub fn gen_addr() -> SocketAddr {
    thread_local! {
        static NEXT_PORT: Cell<u16> = Cell::new(1000);
    }
    let port = NEXT_PORT.with(|cell| cell.replace(cell.get().wrapping_add(1)));

    ([192, 0, 2, 0], port).into()
}

// Generate a NodeId with the given age
pub fn gen_node_id(age: u8) -> NodeId {
    let name = ed25519::gen_name_with_age(age);
    NodeId::new(name, gen_addr())
}

// Generate a NodeId with the given age and prefix
pub fn gen_node_id_in_prefix(age: u8, prefix: Prefix) -> NodeId {
    let name = ed25519::gen_name_with_age(age);
    NodeId::new(prefix.substituted_in(name), gen_addr())
}

// Generate `MyNodeInfo` with the given age and prefix
pub fn gen_info(age: u8, prefix: Option<Prefix>) -> MyNodeInfo {
    MyNodeInfo::new(
        ed25519::gen_keypair(&prefix.unwrap_or_default().range_inclusive(), age),
        gen_addr(),
    )
}

/// Create `elder+adult` Nodes sorted by their names.
///
/// Optionally provide `age_pattern` to create elders with specific ages.
/// If None = elder's age is set to `MIN_ADULT_AGE`
/// If age_pattern.len() == elder, then apply the respective ages to each node
/// If age_pattern.len() < elder, then the last element's value is taken as the age for the remaining nodes.
/// If age_pattern.len() > elder, then the extra elements after `count` are ignored.
pub fn gen_sorted_nodes(
    prefix: &Prefix,
    elder: usize,
    adult: usize,
    elder_age_pattern: Option<&[u8]>,
) -> Vec<MyNodeInfo> {
    let pattern = if let Some(user_pattern) = elder_age_pattern {
        if user_pattern.is_empty() {
            None
        } else if user_pattern.len() < elder {
            let last_element = user_pattern[user_pattern.len() - 1];
            let mut pattern = vec![last_element; elder - user_pattern.len()];
            pattern.extend_from_slice(user_pattern);
            Some(pattern)
        } else {
            Some(Vec::from(user_pattern))
        }
    } else {
        None
    };
    (0..elder)
        .map(|idx| {
            let age = if let Some(pattern) = &pattern {
                pattern[idx]
            } else {
                MIN_ADULT_AGE
            };
            gen_info(age, Some(*prefix))
        })
        .chain((0..adult).map(|_| gen_info(MIN_ADULT_AGE, Some(*prefix))))
        .sorted_by_key(|node| node.age())
        .rev()
        .collect()
}

pub fn section_decision<P: Proposition>(
    secret_key_set: &bls::SecretKeySet,
    proposal: P,
) -> Result<Decision<P>> {
    let n = secret_key_set.threshold() + 1;
    let mut nodes = Vec::from_iter((1..=n).into_iter().map(|idx| {
        let secret = (idx as u8, secret_key_set.secret_key_share(idx));
        Consensus::from(secret, secret_key_set.public_keys(), n)
    }));

    let first_vote = nodes[0]
        .sign_vote(Vote {
            gen: 0,
            ballot: Ballot::Propose(proposal),
            faults: Default::default(),
        })
        .wrap_err("Failed to sign first vote")?;

    let mut votes = vec![nodes[0]
        .cast_vote(first_vote)
        .wrap_err("Failed to cast vote")?];

    while let Some(vote) = votes.pop() {
        for node in &mut nodes {
            match node
                .handle_signed_vote(vote.clone())
                .wrap_err("Failed to handle vote")?
            {
                VoteResponse::WaitingForMoreVotes => (),
                VoteResponse::Broadcast(vote) => votes.push(vote),
            }
        }
    }

    // All nodes have agreed to the same proposal
    assert_eq!(
        BTreeSet::from_iter(nodes.iter().map(|n| {
            if let Some(d) = n.decision.clone() {
                d.proposals
            } else {
                BTreeMap::new()
            }
        }))
        .len(),
        1
    );

    nodes[0]
        .decision
        .clone()
        .wrap_err("We should have seen a decision, this is a bug")
}

struct FakeProofKeyVerifier {}
impl sn_dbc::SpentProofKeyVerifier for FakeProofKeyVerifier {
    type Error = Error;

    fn verify_known_key(&self, _key: &bls::PublicKey) -> std::result::Result<(), Error> {
        Ok(())
    }
}

/// Reissue a new DBC (at a particular amount) from a given input DBC.
///
/// The change DBC will be discarded.
///
/// A spent proof share is generated for the input DBC, but it doesn't go through the complete
/// spending validation process. This should be OK for the testing process.
pub fn reissue_dbc(
    input: &Dbc,
    amount: u64,
    output_owner_sk: &bls::SecretKey,
    sap: &SectionAuthorityProvider,
    section_keys_provider: &SectionKeysProvider,
) -> Result<Dbc> {
    let output_amount = Token::from_nano(amount);
    let input_amount = input.amount_secrets_bearer()?.amount();
    let change_amount = input_amount
        .checked_sub(output_amount)
        .ok_or_else(|| eyre!("The input amount minus the amount must evaluate to a valid value"))?;

    let mut rng = rand::thread_rng();
    let output_owner = Owner::from(output_owner_sk.clone());
    let mut dbc_builder = TransactionBuilder::default()
        .add_input_dbc_bearer(input)?
        .add_output_by_amount(
            output_amount,
            OwnerOnce::from_owner_base(output_owner, &mut rng),
        )
        .add_output_by_amount(
            change_amount,
            OwnerOnce::from_owner_base(input.owner_base().clone(), &mut rng),
        )
        .build(rng)?;
    for (public_key, tx) in dbc_builder.inputs() {
        let public_commitments = get_public_commitments_from_transaction(
            &tx,
            &input.spent_proofs,
            &input.spent_transactions,
        )?;
        let public_commitment: Commitment = public_commitments
            .into_iter()
            .find(|(k, _c)| k == &public_key)
            .map(|(_k, c)| c)
            .ok_or_else(|| eyre!("Found no commitment for Tx input with pubkey: {public_key:?}"))?;

        let spent_proof_share = build_spent_proof_share(
            &public_key,
            &tx,
            None,
            sap,
            section_keys_provider,
            public_commitment,
        )?;
        dbc_builder = dbc_builder
            .add_spent_proof_share(spent_proof_share)
            .add_spent_transaction(tx);
    }
    let verifier = FakeProofKeyVerifier {};
    let output_dbcs = dbc_builder.build(&verifier)?;
    let (output_dbc, ..) = output_dbcs
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("At least one output DBC should have been generated"))?;
    Ok(output_dbc)
}

/// Gets a public key and a transaction that are ready to be used in a spend request.
pub fn get_input_dbc_spend_info(
    input: &Dbc,
    amount: u64,
    output_owner_sk: &bls::SecretKey,
) -> Result<(bls::PublicKey, DbcTransaction)> {
    let output_amount = Token::from_nano(amount);
    let input_amount = input.amount_secrets_bearer()?.amount();
    let change_amount = input_amount
        .checked_sub(output_amount)
        .ok_or_else(|| eyre!("The input amount minus the amount must evaluate to a valid value"))?;

    let mut rng = rand::thread_rng();
    let output_owner = Owner::from(output_owner_sk.clone());
    let dbc_builder = TransactionBuilder::default()
        .add_input_dbc_bearer(input)?
        .add_output_by_amount(
            output_amount,
            OwnerOnce::from_owner_base(output_owner, &mut rng),
        )
        .add_output_by_amount(
            change_amount,
            OwnerOnce::from_owner_base(input.owner_base().clone(), &mut rng),
        )
        .build(rng)?;
    let inputs = dbc_builder.inputs();
    let first = inputs
        .first()
        .ok_or_else(|| eyre!("There must be at least one input on the transaction"))?;
    Ok(first.clone())
}

pub fn assert_lists<I, J, K>(a: I, b: J)
where
    K: fmt::Debug + Eq,
    I: IntoIterator<Item = K>,
    J: IntoIterator<Item = K>,
{
    let vec1: Vec<_> = a.into_iter().collect();
    let mut vec2: Vec<_> = b.into_iter().collect();

    assert_eq!(vec1.len(), vec2.len());

    for item1 in &vec1 {
        let idx2 = vec2
            .iter()
            .position(|item2| item1 == item2)
            .expect("Item not found in second list");

        vec2.swap_remove(idx2);
    }

    assert_eq!(vec2.len(), 0);
}

/// Create a decision for the testing purpose with the given age.
///
/// NOTE: recommended to call this with low `age` (4 or 5), otherwise it might take very long time
/// to complete because it needs to generate a signature with the number of trailing zeroes equal
/// to (or greater that) `age`.
///
// TODO: Rename it to create_test_relocation_trigger.
pub fn create_relocation_trigger(
    sk_set: &bls::SecretKeySet,
    age: u8,
) -> Result<(RelocationTrigger, Decision<NodeState>)> {
    use super::relocation_check;

    loop {
        let node_state =
            NodeState::joined(gen_node_id(MIN_ADULT_AGE), Some(xor_name::rand::random()));
        let decision = section_decision(sk_set, node_state.clone())?;
        let relocation_trigger = RelocationTrigger::new(decision.clone());
        let churn_id = relocation_trigger.churn_id();

        if relocation_check(age, &churn_id) && !relocation_check(age + 1, &churn_id) {
            return Ok((relocation_trigger, decision));
        }
    }
}
