// Copyright 2020 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::{
    account::Account, ActorEvent, ReceivedCredit, ReplicaValidator, TransferInitiated,
    TransferRegistrationSent, TransferValidated, TransferValidationReceived, TransfersSynched,
};
use crdts::Dot;
use itertools::Itertools;
use log::{debug, warn};
use safe_nd::{
    AccountId, DebitAgreementProof, Error, Money, ReplicaEvent, Result, SafeKey, Signature,
    SignatureShare, SignedTransfer, Transfer,
};
use std::collections::{BTreeMap, HashSet};
use threshold_crypto::PublicKeySet;

/// A signature share, with its index in the combined collection.
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct SecretKeyShare {
    /// Index in the combined collection.
    pub index: usize,
    /// Replica signature over the transfer cmd.
    pub secret_key: threshold_crypto::SecretKeyShare,
}

/// The Actor is the part of an AT2 system
/// that initiates transfers, by requesting Replicas
/// to validate them, and then receive the proof of agreement.
/// It also syncs transfers from the Replicas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor<V: ReplicaValidator> {
    id: AccountId,
    client_safe_key: SafeKey,
    /// Set of all transfers impacting a given identity
    account: Account,
    /// Ensures that the actor's transfer
    /// initiations (ValidateTransfer cmd) are sequential.
    next_debit_version: u64,
    /// When a transfer is initiated, validations are accumulated here.
    /// After quorum is reached and proof produced, the set is cleared.
    accumulating_validations: BTreeMap<PublicKeySet, HashSet<TransferValidated>>,
    /// The PK Set of the Replicas
    replicas: PublicKeySet,
    /// The passed in replica_validator, contains the logic from upper layers
    /// for determining if a remote group of Replicas, represented by a PublicKey, is indeed valid.
    replica_validator: V,
}

impl<V: ReplicaValidator> Actor<V> {
    /// Use this ctor for a new instance,
    /// or to rehydrate from events ([see the synch method](Actor::synch)).
    /// Pass in the key set of the replicas of this actor, i.e. our replicas.
    /// Credits to our account are most likely debited at other replicas than our own (the sender's replicas),
    /// The replica_validator lets upper layer decide how to validate those remote replicas (i.e. not our replicas).
    /// If upper layer trusts them, the validator might do nothing but return "true".
    /// If it wants to execute some logic for verifying that the remote replicas are in fact part of the system,
    /// before accepting credits, it then implements that in the replica_validator.
    pub fn new(client_safe_key: SafeKey, replicas: PublicKeySet, replica_validator: V) -> Actor<V> {
        let id = client_safe_key.public_key();
        Actor {
            id,
            client_safe_key,
            replicas,
            replica_validator,
            account: Account::new(id),
            next_debit_version: 0,
            accumulating_validations: Default::default(),
        }
    }

    /// Temp, for test purposes
    pub fn from_snapshot(
        account: Account,
        client_safe_key: SafeKey,
        replicas: PublicKeySet,
        replica_validator: V,
    ) -> Actor<V> {
        let id = client_safe_key.public_key();
        Actor {
            id,
            client_safe_key,
            replicas,
            replica_validator,
            account,
            next_debit_version: 0,
            accumulating_validations: Default::default(),
        }
    }

    /// -----------------------------------------------------------------
    /// ---------------------- Queries ----------------------------------
    /// -----------------------------------------------------------------

    /// Query for the id of the Actor.
    pub fn id(&self) -> AccountId {
        self.id
    }

    /// Query for new credits since specified index.
    pub fn credits_since(&self, index: usize) -> Vec<Transfer> {
        self.account.credits_since(index)
    }

    /// Query for new debits since specified index.
    pub fn debits_since(&self, index: usize) -> Vec<Transfer> {
        self.account.debits_since(index)
    }

    /// Query for the balance of the Actor.
    pub fn balance(&self) -> Money {
        self.account.balance()
    }

    /// -----------------------------------------------------------------
    /// ---------------------- Cmds -------------------------------------
    /// -----------------------------------------------------------------

    /// Step 1. Build a valid cmd for validation of a debit.
    pub fn transfer(&self, amount: Money, to: AccountId) -> Result<TransferInitiated> {
        if to == self.id {
            return Err(Error::from("Sender and recipient are the same"));
        }

        let id = Dot::new(self.id, self.account.next_debit());

        // ensures one debit is completed at a time
        if self.next_debit_version != self.account.next_debit() {
            return Err(Error::from("Current pending debit has not been completed"));
        }
        if self.next_debit_version != id.counter {
            return Err(Error::from("Debit already proposed or out of order"));
        }
        if amount > self.balance() {
            return Err(Error::InsufficientBalance);
        }
        let transfer = Transfer { id, to, amount };
        match self.sign(&transfer) {
            Ok(actor_signature) => {
                let signed_transfer = SignedTransfer {
                    transfer,
                    actor_signature,
                };
                Ok(TransferInitiated { signed_transfer })
            }
            Err(e) => Err(e),
        }
    }

    /// Step 2. Receive validations from Replicas, aggregate the signatures.
    pub fn receive(&self, validation: TransferValidated) -> Result<TransferValidationReceived> {
        // Always verify signature first! (as to not leak any information).
        if !self.verify(&validation).is_ok() {
            return Err(Error::InvalidSignature);
        }
        let signed_transfer = &validation.signed_transfer;
        // check if validation was initiated by this actor
        if self.id != signed_transfer.transfer.id.actor {
            return Err(Error::from("Validation not intended for this actor")); // "validation is not intended for this actor"
        }
        // check if expected this validation
        if self.next_debit_version != signed_transfer.transfer.id.counter {
            return Err(Error::from("Out of order validation"));
        }
        // check if already received
        for (_, validations) in &self.accumulating_validations {
            if validations.contains(&validation) {
                return Err(Error::from("Already received validation"));
            }
        }

        let mut proof = None;
        let accumulating_validations = &self.accumulating_validations;

        let largest_group = accumulating_validations
            .clone()
            .into_iter()
            .max_by_key(|c| c.1.len());
        match largest_group {
            None => (),
            Some((replicas, accumulated)) => {
                // If received validation is made by same set of replicas as this group,
                // and the current count of accumulated is same as the threshold,
                // then we have reached the quorum needed to build the proof. (Quorum = threshold + 1)
                let quorum =
                    accumulated.len() >= replicas.threshold() && replicas == validation.replicas;

                if quorum {
                    // collect sig shares
                    let last_sig = validation.clone().replica_signature;
                    let sig_shares: BTreeMap<_, _> = accumulated
                        .into_iter()
                        .map(|v| v.replica_signature)
                        .map(|s| (s.index, s.share))
                        .chain(vec![(last_sig.index, last_sig.share)])
                        .collect();

                    if let Ok(data) = bincode::serialize(&signed_transfer) {
                        // Combine shares to produce the main signature.
                        let sig = replicas
                            .combine_signatures(&sig_shares)
                            .expect("not enough shares");
                        // Validate the main signature. If the shares were valid, this can't fail.
                        if replicas.public_key().verify(&sig, data) {
                            proof = Some(DebitAgreementProof {
                                signed_transfer: signed_transfer.clone(),
                                debiting_replicas_sig: safe_nd::Signature::Bls(sig),
                                replica_key: replicas,
                            });
                        } // else, we have some corrupt data. (todo: Do we need to act on that fact?)
                    };
                }
            }
        }

        Ok(TransferValidationReceived { validation, proof })
    }

    /// Step 3. Registration of an agreed transfer.
    /// (The actual sending of the registration over the wire is done by upper layer,
    /// only after that, the event is applied to the actor instance.)
    pub fn register(&self, debit_proof: DebitAgreementProof) -> Result<TransferRegistrationSent> {
        // Always verify signature first! (as to not leak any information).
        if !self.verify_debit_proof(&debit_proof).is_ok() {
            return Err(Error::InvalidSignature);
        }
        match self
            .account
            .is_sequential(&debit_proof.signed_transfer.transfer)
        {
            Ok(is_sequential) => {
                if is_sequential {
                    Ok(TransferRegistrationSent { debit_proof })
                } else {
                    Err(Error::from("Non-sequential opertaion")) // "Non-sequential operation"
                }
            }
            Err(_) => {
                warn!("Invalid operation in transfer actor.");
                Err(Error::InvalidOperation)
            } // from this place this code won't happen, but account validates the transfer is actually debits from it's owner.
        }
    }

    /// Step xx. Continuously receiving credits from Replicas via push or pull model, decided by upper layer.
    /// The credits are most likely originating at an Actor whose Replicas are not the same as our Replicas.
    /// That means that the signature on the DebitAgreementProof, is that of some Replicas we don't know.
    /// What we do here is to use the passed in replica_validator, that injects the logic from upper layers
    /// for determining if this remote group of Replicas is indeed valid.
    /// It should consider our Replicas valid as well, for the rare cases when sender replicate to the same group.
    ///
    /// This also ensures that we receive transfers initiated at other Actor instances (same id or other,
    /// i.e. with multiple instances of same Actor we can also sync debits made on other isntances).
    /// Todo: This looks to be handling the case when there is a transfer in flight from this client
    /// (i.e. self.next_debit_version has been incremented, but transfer not yet accumulated).
    /// Just make sure this is 100% the case as well.
    pub fn synch(&self, events: Vec<ReplicaEvent>) -> Result<TransfersSynched> {
        let credits = self.validate_credits(&events);
        let debits = self.validate_debits(events);

        if credits.len() > 0 || debits.len() > 0 {
            Ok(TransfersSynched { credits, debits })
        } else {
            Err(Error::from("No credits or debits found to sync to actor"))
        }
    }

    fn validate_credits(&self, events: &Vec<ReplicaEvent>) -> Vec<ReceivedCredit> {
        let valid_credits: Vec<_> = events
            .into_iter()
            .filter_map(|e| match e {
                ReplicaEvent::TransferPropagated(e) => Some(e),
                _ => None,
            })
            .unique_by(|e| e.id())
            .map(|e| ReceivedCredit {
                debit_proof: e.debit_proof.clone(),
                debiting_replicas: e.debiting_replicas,
            })
            .filter(|_credit| {
                #[cfg(feature = "simulated-payouts")]
                return true;

                #[cfg(not(feature = "simulated-payouts"))]
                self.verify_credit_proof(_credit).is_ok()
            })
            .filter(|credit| self.id == credit.to())
            .filter(|credit| !self.account.contains(&credit.id()))
            .collect();

        valid_credits
    }

    fn validate_debits(&self, events: Vec<ReplicaEvent>) -> Vec<DebitAgreementProof> {
        let mut debits: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ReplicaEvent::TransferRegistered(e) => Some(e),
                _ => None,
            })
            .unique_by(|e| e.id())
            .map(|e| &e.debit_proof)
            .filter(|debit| self.id == debit.from())
            .filter(|debit| debit.id().counter >= self.account.next_debit())
            .filter(|debit| self.verify_debit_proof(debit).is_ok())
            .collect();

        debits.sort_by_key(|t| t.id().counter);

        let mut iter = 0;
        let mut valid_debits = vec![];
        for out in debits {
            let version = out.id().counter;
            let expected_version = iter + self.account.next_debit();
            if version != expected_version {
                break; // since it's sorted, if first is not matching, then no point continuing
            }
            valid_debits.push(out.clone());
            iter += 1;
        }

        valid_debits
    }

    /// -----------------------------------------------------------------
    /// ---------------------- Mutation ---------------------------------
    /// -----------------------------------------------------------------

    /// Mutation of state.
    /// There is no validation of an event, it is assumed to have
    /// been properly validated before raised, and thus anything that breaks is a bug.
    pub fn apply(&mut self, event: ActorEvent) {
        debug!("Applying event {:?}", event);
        match event {
            ActorEvent::TransferInitiated(e) => {
                self.next_debit_version = e.id().counter + 1;
            }
            ActorEvent::TransferValidationReceived(e) => {
                if let Some(_) = e.proof {
                    // if we have a proof, then we have a valid set of replicas (potentially new) to update with
                    self.replicas = e.validation.replicas.clone();
                }
                match self
                    .accumulating_validations
                    .get_mut(&e.validation.replicas)
                {
                    Some(set) => {
                        let _ = set.insert(e.validation.clone());
                    }
                    None => {
                        // Creates if not exists.
                        let mut set = HashSet::new();
                        let _ = set.insert(e.validation.clone());
                        let _ = self
                            .accumulating_validations
                            .insert(e.validation.replicas.clone(), set);
                    }
                }
            }
            ActorEvent::TransferRegistrationSent(e) => {
                self.account.append(e.debit_proof.signed_transfer.transfer);
                self.accumulating_validations.clear();
            }
            ActorEvent::TransfersSynched(e) => {
                for credit in e.credits {
                    // append credits _before_ debits
                    self.account
                        .append(credit.debit_proof.signed_transfer.transfer);
                }
                let any_debits = e.debits.len() > 0;
                for proof in e.debits {
                    // append debits _after_ credits
                    self.account.append(proof.signed_transfer.transfer);
                }
                if any_debits {
                    // set the synchronisation counter
                    self.next_debit_version = self.account.next_debit() - 1;
                }
            }
        };
        // consider event log, to properly be able to reconstruct state from restart
    }

    /// -----------------------------------------------------------------
    /// ---------------------- Private methods --------------------------
    /// -----------------------------------------------------------------

    fn sign(&self, transfer: &Transfer) -> Result<Signature> {
        match bincode::serialize(transfer) {
            Err(_) => Err(Error::NetworkOther("Could not serialise transfer".into())),
            Ok(data) => Ok(self.client_safe_key.sign(&data)),
        }
    }

    /// We verify that we signed the underlying cmd,
    /// and the replica signature against the pk set included in the event.
    /// Note that we use the provided pk set to verify the event.
    /// This might not be the way we want to do it.
    fn verify(&self, event: &TransferValidated) -> Result<()> {
        let cmd = &event.signed_transfer;
        // Check that we signed this.
        if let error @ Err(_) = self.verify_is_our_transfer(cmd) {
            return error;
        }

        self.verify_share(cmd, &event.replica_signature, &event.replicas)
    }

    // Check that the replica signature is valid per the provided public key set.
    // (if we only use this in one place we can move the content to that method)
    fn verify_share<T: serde::Serialize>(
        &self,
        item: T,
        replica_signature: &SignatureShare,
        replicas: &PublicKeySet,
    ) -> Result<()> {
        let sig_share = &replica_signature.share;
        let share_index = replica_signature.index;
        match bincode::serialize(&item) {
            Err(_) => Err(Error::NetworkOther("Could not serialise item".into())),
            Ok(data) => {
                let verified = replicas
                    .public_key_share(share_index)
                    .verify(sig_share, data);
                if verified {
                    Ok(())
                } else {
                    Err(Error::InvalidSignature)
                }
            }
        }
    }

    /// Verify that this is a valid DebitAgreementProof over our cmd.
    fn verify_debit_proof(&self, proof: &DebitAgreementProof) -> Result<()> {
        let cmd = &proof.signed_transfer;
        // Check that we signed this.
        if let error @ Err(_) = self.verify_is_our_transfer(cmd) {
            return error;
        }

        // Check that the proof corresponds to a/the public key set of our Replicas.
        match bincode::serialize(&proof.signed_transfer) {
            Err(_) => Err(Error::NetworkOther("Could not serialise transfer".into())),
            Ok(data) => {
                let public_key = safe_nd::PublicKey::Bls(self.replicas.public_key());
                public_key.verify(&proof.debiting_replicas_sig, &data)
            }
        }
    }

    /// Verify that this is a valid ReceivedCredit.
    #[cfg(not(feature = "simulated-payouts"))]
    fn verify_credit_proof(&self, credit: &ReceivedCredit) -> Result<()> {
        if !self.replica_validator.is_valid(credit.debiting_replicas) {
            return Err(Error::InvalidSignature);
        }
        let proof = &credit.debit_proof;

        // Check that the proof corresponds to a/the public key set of our Replicas.
        match bincode::serialize(&proof.signed_transfer) {
            Err(_) => Err(Error::NetworkOther("Could not serialise transfer".into())),
            Ok(data) => credit
                .debiting_replicas
                .verify(&proof.debiting_replicas_sig, &data),
        }
    }

    /// Check that we signed this.
    fn verify_is_our_transfer(&self, signed_transfer: &SignedTransfer) -> Result<()> {
        match bincode::serialize(&signed_transfer.transfer) {
            Err(_) => Err(Error::NetworkOther("Could not serialise transfer".into())),
            Ok(data) => {
                let actor_sig = self
                    .client_safe_key
                    .public_id()
                    .public_key()
                    .verify(&signed_transfer.actor_signature, data);
                if actor_sig.is_ok() {
                    Ok(())
                } else {
                    Err(Error::InvalidSignature)
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::{
        Account, Actor, ActorEvent, ReplicaValidator, TransferInitiated, TransferRegistrationSent,
    };
    use crdts::Dot;
    use rand::Rng;
    use safe_nd::{
        ClientFullId, DebitAgreementProof, Money, PublicKey, SafeKey, Signature, SignatureShare,
        Transfer, TransferValidated,
    };
    use std::collections::BTreeMap;
    use threshold_crypto::{SecretKey, SecretKeySet};
    struct Validator {}

    impl ReplicaValidator for Validator {
        fn is_valid(&self, replica_group: PublicKey) -> bool {
            true
        }
    }

    #[test]
    fn creates_actor() {
        // Act
        let (_actor, _sk_set) = get_actor_and_replicas_sk_set(10);
    }

    #[test]
    fn initial_state_is_applied() {
        // Act
        let initial_amount = 10;
        let (actor, _sk_set) = get_actor_and_replicas_sk_set(initial_amount);
        let credits = actor.credits_since(0);
        let debits = actor.debits_since(0);
        assert!(debits.len() == 0);
        assert!(credits.len() == 1);
        assert!(credits[0].amount == Money::from_nano(initial_amount));
        assert!(actor.balance() == Money::from_nano(initial_amount));
    }

    #[test]
    fn initiates_transfers() {
        // Act
        let (actor, _sk_set) = get_actor_and_replicas_sk_set(10);
        let debit = get_debit(&actor);
        let mut actor = actor;
        actor.apply(ActorEvent::TransferInitiated(debit))
    }

    #[test]
    fn can_apply_completed_transfer() {
        // Act
        let (actor, sk_set) = get_actor_and_replicas_sk_set(15);
        let debit = get_debit(&actor);
        let mut actor = actor;
        actor.apply(ActorEvent::TransferInitiated(debit.clone()));
        let transfer_event = get_transfer_registration_sent(debit, &sk_set);
        actor.apply(ActorEvent::TransferRegistrationSent(transfer_event));
        assert_eq!(Money::from_nano(5), actor.balance())
    }

    #[test]
    fn can_apply_completed_transfers_in_succession() {
        // Act
        let (actor, sk_set) = get_actor_and_replicas_sk_set(22);
        let debit = get_debit(&actor);
        let mut actor = actor;
        actor.apply(ActorEvent::TransferInitiated(debit.clone()));
        let transfer_event = get_transfer_registration_sent(debit, &sk_set);
        actor.apply(ActorEvent::TransferRegistrationSent(transfer_event));

        assert_eq!(Money::from_nano(12), actor.balance()); // 22 - 10

        let debit2 = get_debit(&actor);
        actor.apply(ActorEvent::TransferInitiated(debit2.clone()));
        let transfer_event = get_transfer_registration_sent(debit2, &sk_set);
        actor.apply(ActorEvent::TransferRegistrationSent(transfer_event));

        assert_eq!(Money::from_nano(2), actor.balance()); // 22 - 10 - 10
    }

    #[test]
    fn can_return_proof_for_validated_transfers() {
        let (actor, sk_set) = get_actor_and_replicas_sk_set(22);
        let debit = get_debit(&actor);
        let mut actor = actor;

        let validations = get_transfer_validation_vec(debit, &sk_set);

        // 7 elders and validations
        for i in 0..7 {
            let transfer_validation = actor.receive(validations[i].clone()).unwrap();

            if i < 1
            // threshold is 1
            {
                assert_eq!(transfer_validation.clone().proof, None);
            } else {
                assert_ne!(transfer_validation.proof, None);
            }

            actor.apply(ActorEvent::TransferValidationReceived(
                transfer_validation.clone(),
            ));
        }
    }

    fn get_debit(actor: &Actor<Validator>) -> TransferInitiated {
        match actor.transfer(Money::from_nano(10), get_random_pk()) {
            Ok(event) => event,
            Err(e) => panic!(e),
        }
    }

    /// returns a vec of validated transfers from the sk_set 'replicas'
    fn get_transfer_validation_vec(
        transfer: TransferInitiated,
        sk_set: &SecretKeySet,
    ) -> Vec<TransferValidated> {
        let signed_transfer = transfer.signed_transfer.clone();
        let serialized_signed_transfer = bincode::serialize(&signed_transfer.clone()).unwrap();
        let sk_shares: Vec<_> = (0..7).map(|i| sk_set.secret_key_share(i)).collect();
        let pk_set = sk_set.public_keys();

        let sig_shares: BTreeMap<_, _> = (0..7)
            .map(|i| (i, sk_shares[i].sign(serialized_signed_transfer.clone())))
            .collect();

        let mut validated_transfers = vec![];
        for (i, sig_share) in &sig_shares {
            assert!(pk_set
                .public_key_share(*i)
                .verify(sig_share, serialized_signed_transfer.clone()));

            validated_transfers.push(TransferValidated {
                signed_transfer: signed_transfer.clone(),
                replica_signature: SignatureShare {
                    index: *i,
                    share: sig_share.clone(),
                },
                replicas: pk_set.clone(),
            })
        }

        validated_transfers
    }

    fn get_transfer_registration_sent(
        transfer: TransferInitiated,
        sk_set: &SecretKeySet,
    ) -> TransferRegistrationSent {
        let signed_transfer = transfer.signed_transfer.clone();
        let serialized_signed_transfer = bincode::serialize(&signed_transfer.clone()).unwrap();
        let sk_shares: Vec<_> = (0..6).map(|i| sk_set.secret_key_share(i)).collect();
        let pk_set = sk_set.public_keys();

        // Create four signature shares for the message.
        let sig_shares: BTreeMap<_, _> = (0..4)
            .map(|i| (i, sk_shares[i].sign(serialized_signed_transfer.clone())))
            .collect();

        // // Validate the signature shares.
        for (i, sig_share) in &sig_shares {
            assert!(pk_set
                .public_key_share(*i)
                .verify(sig_share, serialized_signed_transfer.clone()));
        }

        // Combine them to produce the main signature.
        let sig = pk_set
            .combine_signatures(&sig_shares)
            .expect("not enough shares");

        // Validate the main signature. If the shares were valid, this can't fail.
        assert!(pk_set.public_key().verify(&sig, serialized_signed_transfer));

        let debiting_replicas_sig = Signature::Bls(sig);
        let debit_agreement_proof = DebitAgreementProof {
            signed_transfer: transfer.signed_transfer,
            debiting_replicas_sig,
            replica_key: pk_set,
        };

        TransferRegistrationSent {
            debit_proof: debit_agreement_proof,
        }
    }

    fn get_actor_and_replicas_sk_set(amount: u64) -> (Actor<Validator>, SecretKeySet) {
        let mut rng = rand::thread_rng();
        let client_safe_key = SafeKey::client(ClientFullId::new_ed25519(&mut rng));
        let client_pubkey = client_safe_key.public_key();
        let bls_secret_key = SecretKeySet::random(1, &mut rng);
        let replicas_id = bls_secret_key.public_keys();
        let balance = Money::from_nano(amount);
        let sender = Dot::new(get_random_pk(), 0);
        let transfer = get_transfer(sender, client_pubkey, balance);
        let replica_validator = Validator {};
        let mut account = Account::new(transfer.to);
        account.append(transfer);
        let actor = Actor::from_snapshot(account, client_safe_key, replicas_id, replica_validator);
        (actor, bls_secret_key)
    }

    fn get_transfer(from: Dot<PublicKey>, to: PublicKey, amount: Money) -> Transfer {
        Transfer {
            id: from,
            to,
            amount,
        }
    }

    fn get_random_dot() -> Dot<PublicKey> {
        Dot::new(get_random_pk(), 0)
    }

    fn get_random_pk() -> PublicKey {
        PublicKey::from(SecretKey::random().public_key())
    }
}
