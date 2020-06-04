// Copyright 2020 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

//! Implementation of Transfers in the SAFE Network.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/maidsafe/QA/master/Images/maidsafe_logo.png",
    html_favicon_url = "https://maidsafe.net/img/favicon.ico",
    test(attr(forbid(warnings)))
)]
// For explanation of lint checks, run `rustc -W help`.
#![forbid(unsafe_code)]
#![warn(
    missing_docs,
    trivial_casts,
    trivial_numeric_casts,
    unused_extern_crates,
    unused_import_braces,
    unused_qualifications,
    unused_results
)]

mod account;
mod actor;
mod replica;

pub use self::{
    account::Account, actor::Actor as TransferActor, replica::Replica as TransferReplica,
};

use safe_nd::{DebitAgreementProof, ReplicaEvent, SignedTransfer, TransferValidated};
use serde::{Deserialize, Serialize};

/// A received credit, contains the DebitAgreementProof from the sender Replicas,
/// as well as the public key of those Replicas, for us to verify that they are valid Replicas.
#[derive(Clone, Hash, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Debug)]
pub struct ReceivedCredit {
    /// The sender's aggregated Replica signatures of the sender debit.
    pub debit_proof: DebitAgreementProof,
    /// The public key of the signing Replicas.
    pub signing_replicas: threshold_crypto::PublicKey,
}

// ------------------------------------------------------------
//                      Actor
// ------------------------------------------------------------

/// An implementation of the ReplicaValidator, should contain the logic from upper layers
/// for determining if a remote group of Replicas, represented by a PublicKey, is indeed valid.
/// This is logic from the membership part of the system, and thus handled by the upper layers
/// membership implementation.
pub trait ReplicaValidator {
    /// Determines if a remote group of Replicas, represented by a PublicKey, is indeed valid.
    fn is_valid(&self, replica_group: threshold_crypto::PublicKey) -> bool;
}

/// Events raised by the Actor.
#[derive(Clone, Hash, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Debug)]
pub enum ActorEvent {
    /// Raised when a request to create
    /// a transfer validation cmd for Replicas,
    /// has been successful (valid on local state).
    TransferInitiated(TransferInitiated),
    /// Raised when an Actor receives a Replica transfer validation.
    TransferValidationReceived(TransferValidationReceived),
    /// Raised when the Actor has accumulated a
    /// quorum of validations, and produced a RegisterTransfer cmd
    /// for sending to Replicas.
    TransferRegistrationSent(TransferRegistrationSent),
    /// Raised when the Actor has received
    /// unknown credits on querying Replicas.
    CreditsReceived(CreditsReceived),
    /// Raised when the Actor has received
    /// unknown debits on querying Replicas.
    DebitsReceived(DebitsReceived),
}

/// This event is raised by the Actor after having
/// successfully created a transfer cmd to send to the
/// Replicas for validation.
#[derive(Clone, Hash, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Debug)]
pub struct TransferInitiated {
    signed_transfer: SignedTransfer,
}

/// Raised when a Replica responds with
/// a successful validation of a transfer.
#[derive(Clone, Hash, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Debug)]
pub struct TransferValidationReceived {
    /// The event raised by a Replica.
    validation: TransferValidated,
    /// Added when quorum of validations
    /// have been received from Replicas.
    proof: Option<DebitAgreementProof>,
}

/// Raised when the Actor has accumulated a
/// quorum of validations, and produced a RegisterTransfer cmd
/// for sending to Replicas.
#[derive(Clone, Hash, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Debug)]
pub struct TransferRegistrationSent {
    debit_proof: DebitAgreementProof,
}

/// Raised when the Actor has received
/// credits that its Replicas were holding upon
/// the propagation of them from a remote group of Replicas.
#[derive(Clone, Hash, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Debug)]
pub struct CreditsReceived {
    /// Credits we don't have locally.
    credits: Vec<ReceivedCredit>,
}

/// Raised when an Actor instance has received
/// unknown debits that its Replicas were holding
/// upon the registration of them from another
/// instance of the same Actor.
#[derive(Clone, Hash, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Debug)]
pub struct DebitsReceived {
    /// The debits we don't have locally.
    debits: Vec<DebitAgreementProof>,
}

mod test {
    use crate::{
        actor::Actor, replica::Replica, Account, ActorEvent, ReceivedCredit, ReplicaEvent,
        ReplicaValidator,
    };
    use crdts::{
        quickcheck::{quickcheck, TestResult},
        Dot,
    };
    use rand::Rng;
    use safe_nd::{AccountId, ClientFullId, Money, PublicKey, Transfer};
    use std::collections::{HashMap, HashSet};
    use threshold_crypto::{PublicKeySet, SecretKey, SecretKeySet, SecretKeyShare};

    #[derive(Debug, Clone)]
    struct Validator {}

    impl ReplicaValidator for Validator {
        fn is_valid(&self, replica_group: threshold_crypto::PublicKey) -> bool {
            true
        }
    }

    #[test]
    fn transfer() {
        send_between_replica_groups(100, 10, 2, 3, 0, 1);
    }

    #[test]
    fn quickcheck_transfer() {
        quickcheck(send_between_replica_groups as fn(u64, u64, u8, u8, u8, u8) -> TestResult);
    }

    fn send_between_replica_groups(
        sender_balance: u64,
        recipient_balance: u64,
        group_count: u8,
        replica_count: u8,
        sender_index: u8,
        recipient_index: u8,
    ) -> TestResult {
        // --- Filter ---
        if 0 >= sender_balance
            || 0 >= group_count
            || 2 >= replica_count
            || sender_index >= group_count
            || recipient_index >= group_count
            || sender_index == recipient_index
        {
            return TestResult::discard();
        }
        // --- Arrange ---
        let recipient_final = sender_balance + recipient_balance;
        let group_keys = get_replica_group_keys(group_count, replica_count);
        let sender_group = group_keys.get(&sender_index).unwrap().clone();
        let recipient_group = group_keys.get(&recipient_index).unwrap().clone();

        let mut sender = get_actor(sender_balance, sender_group.index, sender_group.id);
        let mut recipient = get_actor(recipient_balance, recipient_group.index, recipient_group.id);
        let mut replica_groups =
            get_replica_groups(group_keys, vec![sender.clone(), recipient.clone()]);

        let transfer = sender
            .actor
            .transfer(sender.actor.balance(), recipient.actor.id())
            .unwrap();
        sender
            .actor
            .apply(ActorEvent::TransferInitiated(transfer.clone()));

        let mut debit_proof = None;
        let mut sender_replicas_pubkey = None;

        // --- Act ---
        // Validate at Sender Replicas
        match find_group(sender_index, &mut replica_groups) {
            None => panic!("group not found!"),
            Some(replica_group) => {
                sender_replicas_pubkey = Some(replica_group.id.public_key());
                for replica in &mut replica_group.replicas {
                    let validated = replica.validate(transfer.signed_transfer.clone()).unwrap();
                    replica.apply(ReplicaEvent::TransferValidated(validated.clone()));
                    let validation_received = sender.actor.receive(validated).unwrap();
                    sender.actor.apply(ActorEvent::TransferValidationReceived(
                        validation_received.clone(),
                    ));
                    if let Some(proof) = validation_received.proof {
                        let registered = sender.actor.register(proof.clone()).unwrap();
                        sender
                            .actor
                            .apply(ActorEvent::TransferRegistrationSent(registered));
                        debit_proof = Some(proof);
                    }
                }
            }
        }

        if debit_proof.is_none() {
            println!(
                "No debit proof! sender_balance: {},
            recipient_balance: {},
            group_count: {},
            replica_count: {},
            sender_index: {},
            recipient_index: {},",
                sender_balance,
                recipient_balance,
                group_count,
                replica_count,
                sender_index,
                recipient_index
            )
        }

        // Register at Sender Replicas
        match find_group(sender_index, &mut replica_groups) {
            None => panic!("group not found!"),
            Some(replica_group) => {
                for replica in &mut replica_group.replicas {
                    let registered = replica.register(&debit_proof.clone().unwrap()).unwrap();
                    replica.apply(ReplicaEvent::TransferRegistered(registered));
                }
            }
        }

        // Propagate to Recipient Replicas
        let credits = replica_groups
            .iter_mut()
            .filter(|c| c.index == recipient_index)
            .map(|c| {
                c.replicas.iter_mut().map(|replica| {
                    let propagated = replica
                        .receive_propagated(&debit_proof.clone().unwrap())
                        .unwrap();
                    replica.apply(ReplicaEvent::TransferPropagated(propagated.clone()));
                    ReceivedCredit {
                        debit_proof: propagated.debit_proof,
                        signing_replicas: sender_replicas_pubkey.unwrap(),
                    }
                })
            })
            .flatten()
            .collect::<HashSet<ReceivedCredit>>()
            .into_iter()
            .collect::<Vec<ReceivedCredit>>();

        let credits_received = recipient.actor.receive_credits(credits).unwrap();
        recipient
            .actor
            .apply(ActorEvent::CreditsReceived(credits_received));

        // --- Assert ---

        // Actor has correct balance
        assert!(sender.actor.balance() == Money::zero());
        assert!(recipient.actor.balance() == Money::from_nano(recipient_final));

        // Replicas of the sender have correct balance
        replica_groups
            .iter_mut()
            .filter(|c| c.index == sender_index)
            .map(|c| {
                c.replicas
                    .iter_mut()
                    .map(|replica| replica.balance(&sender.actor.id()).unwrap())
            })
            .flatten()
            .for_each(|balance| assert!(balance == Money::zero()));

        // Replicas of the recipient have correct balance
        replica_groups
            .iter_mut()
            .filter(|c| c.index == recipient_index)
            .map(|c| {
                c.replicas
                    .iter_mut()
                    .map(|replica| replica.balance(&recipient.actor.id()).unwrap())
            })
            .flatten()
            .for_each(|balance| assert!(balance == Money::from_nano(recipient_final)));

        TestResult::passed()
    }

    fn find_group(index: u8, replica_groups: &mut Vec<ReplicaGroup>) -> Option<&mut ReplicaGroup> {
        for replica_group in replica_groups {
            if replica_group.index == index {
                return Some(replica_group);
            }
        }
        None
    }

    // Create n replica groups, with k replicas in each
    fn get_replica_group_keys(group_count: u8, replica_count: u8) -> HashMap<u8, ReplicaGroupKeys> {
        let mut rng = rand::thread_rng();
        let mut groups = HashMap::new();
        for i in 0..group_count {
            let threshold = (2 * replica_count / 3) - 1;
            let bls_secret_key = SecretKeySet::random(threshold as usize, &mut rng);
            let peers = bls_secret_key.public_keys();
            let mut shares = vec![];
            for j in 0..replica_count {
                let share = bls_secret_key.secret_key_share(j as usize);
                shares.push((share, j as usize));
            }
            let _ = groups.insert(
                i,
                ReplicaGroupKeys {
                    index: i,
                    id: peers,
                    keys: shares,
                },
            );
        }
        groups
    }

    fn get_replica_groups(
        group_keys: HashMap<u8, ReplicaGroupKeys>,
        accounts: Vec<TestActor>,
    ) -> Vec<ReplicaGroup> {
        let mut other_groups_keys = HashMap::new();
        for (i, _) in group_keys.clone() {
            let other = group_keys
                .clone()
                .into_iter()
                .filter(|(c, _)| *c != i)
                .map(|(_, group_keys)| group_keys.id)
                .collect::<HashSet<PublicKeySet>>();
            let _ = other_groups_keys.insert(i, other);
        }

        let mut replica_groups = vec![];
        for (i, other) in &other_groups_keys {
            let group_accounts = accounts
                .clone()
                .into_iter()
                .filter(|c| c.replica_group == *i)
                .map(|c| (c.actor.id(), c.account_clone.clone()))
                .collect::<HashMap<AccountId, Account>>();

            let mut replicas = vec![];
            let group = group_keys[i].clone();
            for (secret_key, index) in group.keys {
                let peer_replicas = group.id.clone();
                let other_groups = other.clone();
                let accounts = group_accounts.clone();
                let pending_debits = Default::default();
                let replica = Replica::from_snapshot(
                    secret_key,
                    index,
                    peer_replicas,
                    other_groups,
                    accounts,
                    pending_debits,
                );
                replicas.push(replica);
            }
            let _ = replica_groups.push(ReplicaGroup {
                index: *i,
                id: group.id,
                replicas,
            });
        }
        replica_groups
    }

    fn get_actor(balance: u64, replica_group: u8, replicas_id: PublicKeySet) -> TestActor {
        let mut rng = rand::thread_rng();
        let client_id = ClientFullId::new_ed25519(&mut rng);
        let to = *client_id.public_id().public_key();
        let amount = Money::from_nano(balance);
        let sender = Dot::new(get_random_pk(), 0);
        let transfer = Transfer {
            id: sender,
            to,
            amount,
        };
        let replica_validator = Validator {};
        match Actor::new(client_id, transfer.clone(), replicas_id, replica_validator) {
            None => panic!(),
            Some(actor) => TestActor {
                actor,
                account_clone: Account::new(transfer),
                replica_group,
            },
        }
    }

    fn get_random_pk() -> PublicKey {
        PublicKey::from(SecretKey::random().public_key())
    }

    #[derive(Debug, Clone)]
    struct TestActor {
        actor: Actor<Validator>,
        account_clone: Account,
        replica_group: u8,
    }

    #[derive(Debug, Clone)]
    struct ReplicaGroup {
        index: u8,
        id: PublicKeySet,
        replicas: Vec<Replica>,
    }

    #[derive(Debug, Clone)]
    struct ReplicaGroupKeys {
        index: u8,
        id: PublicKeySet,
        keys: Vec<(SecretKeyShare, usize)>,
    }
}
