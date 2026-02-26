#![cfg_attr(not(feature = "std"), no_std, no_main)]

/// # 52F Protocol — Sequencer Satellite
///
/// **Role:** Mathematical logic layer. Stores the last 52 participant addresses
/// per epoch, executes the MOD-1000 collision algorithm to identify winners,
/// and distributes the 90% yield pulled from the Token Engine.
///
/// **Architecture:**
/// ```text
///   [Token Engine] ──EpochReady event──► [Sequencer Satellite]
///         ▲                                       │
///         └─────── pull_prize_tax() XCC ──────────┘
/// ```
///
/// The Satellite is the *only* contract authorised to call `pull_prize_tax`
/// on the Token Engine.  It never holds $QF$ between epochs — all pulled
/// yield is immediately distributed to winners or rolled over as a credit.
///
/// **Compatibility:** ink! v6 / PolkaVM (`pallet-revive`).
#[ink::contract]
mod sequencer_satellite {
    use ink::prelude::vec::Vec;
    use ink::storage::Mapping;
    use ink::env::call::{build_call, ExecutionInput, Selector};

    type Address = <ink::env::DefaultEnvironment as ink::env::Environment>::AccountId;
    use ink::primitives::U256;

    // =========================================================================
    // CONSTANTS
    // =========================================================================

    /// Number of entries per epoch (matches Token Engine `EPOCH_SIZE`).
    pub const EPOCH_SIZE: u32 = 52;

    /// Modulus for the collision hash. Two entries "collide" when their
    /// truncated hash values are equal under MOD 1000.
    pub const COLLISION_MODULUS: u64 = 1_000;

    // =========================================================================
    // STORAGE
    // =========================================================================

    #[ink(storage)]
    pub struct SequencerSatellite {
        /// Deployer / admin.
        owner: Address,

        /// Address of the Token Engine contract.
        token_engine: Address,

        // ── Epoch state ───────────────────────────────────────────────────
        /// Current epoch identifier (mirrors Token Engine).
        current_epoch_id: u32,

        /// Participant addresses registered in the current epoch.
        /// Bounded at EPOCH_SIZE entries; reset on every epoch flush.
        epoch_participants: Vec<Address>,

        // ── Roll-over pot ─────────────────────────────────────────────────
        /// Accumulated yield from epochs where no collision was detected.
        /// Added to the prize pot on the next winning epoch.
        rollover_pot: U256,

        // ── Historical ledger ─────────────────────────────────────────────
        /// Total winnings paid out to each address, all-time.
        total_winnings: Mapping<Address, U256>,

        /// Epoch ID → winning addresses (for on-chain auditability).
        epoch_winners: Mapping<u32, Vec<Address>>,

        paused: bool,
    }

    // =========================================================================
    // EVENTS
    // =========================================================================

    /// Emitted when the Satellite receives an epoch trigger and finds winners.
    #[ink(event)]
    pub struct CollisionDetected {
        #[ink(topic)]
        epoch_id: u32,
        winner_count: u32,
        yield_per_winner: U256,
        total_yield: U256,
    }

    /// Emitted when no collision is found and the yield rolls over.
    #[ink(event)]
    pub struct EpochRolledOver {
        #[ink(topic)]
        epoch_id: u32,
        rolled_amount: U256,
        cumulative_rollover: U256,
    }

    /// Emitted when a participant is registered for the current epoch.
    #[ink(event)]
    pub struct ParticipantRegistered {
        #[ink(topic)]
        epoch_id: u32,
        #[ink(topic)]
        participant: Address,
        slot: u32,
    }

    // =========================================================================
    // ERRORS
    // =========================================================================

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        /// Caller is not the contract owner.
        NotOwner,
        /// The epoch is already full (52 participants registered).
        EpochFull,
        /// Arithmetic overflow.
        Overflow,
        /// Cross-contract call to the Token Engine failed.
        TokenEngineCallFailed,
        /// The Token Engine returned an empty yield (pot was zero).
        YieldZero,
        /// A native value transfer to a winner failed.
        WinnerTransferFailed,
        /// Contract is paused.
        ContractPaused,
        /// Epoch ID mismatch — stale trigger ignored.
        StaleEpochTrigger,
    }

    // =========================================================================
    // TOKEN ENGINE INTERFACE (Cross-Contract)
    // =========================================================================

    /// Minimal interface used to call `pull_prize_tax` on the Token Engine.
    #[ink::trait_definition]
    pub trait TokenEngineInterface {
        /// Pull the prize pot. Returns the 90% yield amount sent to the caller.
        #[ink(message)]
        fn pull_prize_tax(&mut self) -> Result<U256, crate::sequencer_satellite::Error>;
    }

    // =========================================================================
    // IMPLEMENTATION
    // =========================================================================

    impl SequencerSatellite {
        // ---------------------------------------------------------------------
        // Constructor
        // ---------------------------------------------------------------------

        #[ink(constructor)]
        pub fn new(token_engine: Address) -> Self {
            Self {
                owner: Self::env().caller(),
                token_engine,
                current_epoch_id: 0,
                epoch_participants: Vec::new(),
                rollover_pot: U256::ZERO,
                total_winnings: Mapping::default(),
                epoch_winners: Mapping::default(),
                paused: false,
            }
        }

        // =====================================================================
        // EPOCH ENTRY — Register Participant
        // =====================================================================

        /// Register a participant address for the current epoch.
        ///
        /// Called by the Token Engine (or a trusted relay) each time a valid
        /// transaction occurs.  In production this would be restricted to the
        /// Token Engine address; left open for testnet flexibility.
        ///
        /// Once 52 slots are filled the function silently returns `EpochFull`.
        /// The owner calls `flush_epoch` to resolve the epoch.
        #[ink(message)]
        pub fn register_participant(&mut self, participant: Address) -> Result<(), Error> {
            self.assert_not_paused()?;

            if self.epoch_participants.len() as u32 >= EPOCH_SIZE {
                return Err(Error::EpochFull);
            }

            let slot = self.epoch_participants.len() as u32;
            self.epoch_participants.push(participant);

            self.env().emit_event(ParticipantRegistered {
                epoch_id: self.current_epoch_id,
                participant,
                slot,
            });

            Ok(())
        }

        // =====================================================================
        // EPOCH FLUSH — Collision Detection & Distribution
        // =====================================================================

        /// Flush the current epoch: run collision detection, pull yield from
        /// the Token Engine, and distribute to winners (or roll over).
        ///
        /// **Algorithm (MOD-1000 Collision):**
        /// 1. For each of the 52 participant addresses, derive a `u64` bucket
        ///    via `address_to_bucket(addr) % 1000`.
        /// 2. Any address whose bucket value appears *more than once* is a
        ///    "collision winner."
        /// 3. If ≥1 winner exists: pull yield from Token Engine (XCC), split
        ///    equally among all winners, distribute.
        /// 4. If no winners: pull yield, add to `rollover_pot`, carry forward.
        ///
        /// # Caller
        /// Owner only (this is a manual flush trigger for testnet; production
        /// may use an off-chain keeper or an automated on-chain relay).
        #[ink(message)]
        pub fn flush_epoch(&mut self, expected_epoch_id: u32) -> Result<(), Error> {
            self.assert_not_paused()?;
            self.only_owner()?;

            // Guard against stale / replayed triggers.
            if expected_epoch_id != self.current_epoch_id {
                return Err(Error::StaleEpochTrigger);
            }

            let participants = self.epoch_participants.clone();
            let winners = self.detect_collisions(&participants);

            // ── Pull yield from Token Engine (XCC) ───────────────────────
            let pulled_yield = self.call_pull_prize_tax()?;

            if pulled_yield.is_zero() {
                return Err(Error::YieldZero);
            }

            let total_yield = pulled_yield
                .checked_add(self.rollover_pot)
                .ok_or(Error::Overflow)?;

            let epoch_id = self.current_epoch_id;

            if winners.is_empty() {
                // No collision — roll the pot forward.
                self.rollover_pot = total_yield;

                self.env().emit_event(EpochRolledOver {
                    epoch_id,
                    rolled_amount: pulled_yield,
                    cumulative_rollover: self.rollover_pot,
                });
            } else {
                // Collision found — distribute equally.
                self.rollover_pot = U256::ZERO;

                let winner_count = U256::from(winners.len() as u64);
                let yield_per_winner = total_yield
                    .checked_div(winner_count)
                    .ok_or(Error::Overflow)?;

                // Dust (integer division remainder) remains in the contract
                // and is picked up by the next epoch as an implicit micro-rollover.

                for winner in &winners {
                    self.env()
                        .transfer(*winner, yield_per_winner)
                        .map_err(|_| Error::WinnerTransferFailed)?;

                    let previous = self.total_winnings.get(winner).unwrap_or(U256::ZERO);
                    let updated = previous
                        .checked_add(yield_per_winner)
                        .ok_or(Error::Overflow)?;
                    self.total_winnings.insert(winner, &updated);
                }

                self.epoch_winners.insert(epoch_id, &winners);

                self.env().emit_event(CollisionDetected {
                    epoch_id,
                    winner_count: winners.len() as u32,
                    yield_per_winner,
                    total_yield,
                });
            }

            // ── Reset epoch state ─────────────────────────────────────────
            self.epoch_participants.clear();
            self.current_epoch_id = self.current_epoch_id.saturating_add(1);

            Ok(())
        }

        // =====================================================================
        // INTERNAL — Collision Detection
        // =====================================================================

        /// Derive a `u64` bucket for an address using the first 8 bytes of the
        /// H160 big-endian representation, then apply MOD 1000.
        ///
        /// This is deterministic, gas-cheap, and requires no external oracle.
        fn address_to_bucket(addr: Address) -> u64 {
            let bytes: [u8; 20] = addr.into();
            // Take the first 8 bytes as a big-endian u64.
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes[0..8]);
            let raw = u64::from_be_bytes(buf);
            raw % COLLISION_MODULUS
        }

        /// Return all addresses that share a MOD-1000 bucket with at least one
        /// other participant in the epoch slice.
        ///
        /// O(n) using a counting array (n ≤ 52, modulus = 1 000).
        fn detect_collisions(&self, participants: &[Address]) -> Vec<Address> {
            // bucket_count[i] = number of participants whose bucket == i
            let mut bucket_count = [0u16; COLLISION_MODULUS as usize];

            for addr in participants {
                let bucket = Self::address_to_bucket(*addr) as usize;
                bucket_count[bucket] = bucket_count[bucket].saturating_add(1);
            }

            // Collect addresses whose bucket was hit more than once.
            participants
                .iter()
                .filter(|addr| {
                    let bucket = Self::address_to_bucket(**addr) as usize;
                    bucket_count[bucket] > 1
                })
                .copied()
                .collect()
        }

        // =====================================================================
        // INTERNAL — Cross-Contract Call
        // =====================================================================

        fn call_pull_prize_tax(&self) -> Result<U256, Error> {
            // ink! v6 XCC: build_call → try_invoke for safe error handling.
            let result: Result<Result<U256, crate::sequencer_satellite::Error>, ink::env::Error> =
                build_call::<ink::env::DefaultEnvironment>()
                    .call(self.token_engine)
                    .exec_input(
                        ExecutionInput::new(Selector::new(ink::selector_bytes!("pull_prize_tax"))),
                    )
                    .returns::<Result<U256, crate::sequencer_satellite::Error>>()
                    .try_invoke();

            match result {
                Ok(Ok(amount)) => Ok(amount),
                _ => Err(Error::TokenEngineCallFailed),
            }
        }

        // =====================================================================
        // VIEW FUNCTIONS
        // =====================================================================

        #[ink(message)]
        pub fn get_epoch_id(&self) -> u32 {
            self.current_epoch_id
        }

        #[ink(message)]
        pub fn get_participant_count(&self) -> u32 {
            self.epoch_participants.len() as u32
        }

        #[ink(message)]
        pub fn get_rollover_pot(&self) -> U256 {
            self.rollover_pot
        }

        #[ink(message)]
        pub fn get_total_winnings(&self, addr: Address) -> U256 {
            self.total_winnings.get(addr).unwrap_or(U256::ZERO)
        }

        #[ink(message)]
        pub fn get_epoch_winners(&self, epoch_id: u32) -> Vec<Address> {
            self.epoch_winners.get(epoch_id).unwrap_or_default()
        }

        #[ink(message)]
        pub fn get_token_engine(&self) -> Address {
            self.token_engine
        }

        #[ink(message)]
        pub fn preview_buckets(&self) -> Vec<(Address, u64)> {
            self.epoch_participants
                .iter()
                .map(|addr| (*addr, Self::address_to_bucket(*addr)))
                .collect()
        }

        // =====================================================================
        // ADMIN
        // =====================================================================

        #[ink(message)]
        pub fn set_token_engine(&mut self, addr: Address) -> Result<(), Error> {
            self.only_owner()?;
            self.token_engine = addr;
            Ok(())
        }

        #[ink(message)]
        pub fn set_paused(&mut self, paused: bool) -> Result<(), Error> {
            self.only_owner()?;
            self.paused = paused;
            Ok(())
        }

        // =====================================================================
        // ACCESS CONTROL
        // =====================================================================

        fn only_owner(&self) -> Result<(), Error> {
            if self.env().caller() != self.owner {
                return Err(Error::NotOwner);
            }
            Ok(())
        }

        fn assert_not_paused(&self) -> Result<(), Error> {
            if self.paused {
                return Err(Error::ContractPaused);
            }
            Ok(())
        }
    }

    // =========================================================================
    // UNIT TESTS
    // =========================================================================

    #[cfg(test)]
    mod tests {
        use super::*;
        use ink::env::{test, DefaultEnvironment};

        type Env = DefaultEnvironment;

        fn accounts() -> test::DefaultAccounts<Env> {
            test::default_accounts::<Env>()
        }

        fn set_caller(addr: Address) {
            test::set_caller::<Env>(addr);
        }

        fn deploy() -> SequencerSatellite {
            let accs = accounts();
            set_caller(accs.alice);
            // Use charlie as a mock Token Engine address.
            SequencerSatellite::new(accs.charlie)
        }

        // ── Participant registration ───────────────────────────────────────────

        #[ink::test]
        fn register_increments_count() {
            let mut sat = deploy();
            let accs = accounts();
            sat.register_participant(accs.bob).unwrap();
            sat.register_participant(accs.django).unwrap();
            assert_eq!(sat.get_participant_count(), 2);
        }

        #[ink::test]
        fn register_rejects_after_52_slots() {
            let mut sat = deploy();
            let accs = accounts();

            // Fill all 52 slots with the same address (acceptable in tests).
            for _ in 0..52 {
                sat.register_participant(accs.bob).unwrap();
            }

            // The 53rd registration must fail.
            let result = sat.register_participant(accs.alice);
            assert_eq!(result, Err(Error::EpochFull));
        }

        // ── Bucket / collision logic ───────────────────────────────────────────

        #[ink::test]
        fn bucket_is_below_modulus() {
            let accs = accounts();
            let bucket = SequencerSatellite::address_to_bucket(accs.alice);
            assert!(bucket < COLLISION_MODULUS, "bucket must be < 1000");
        }

        #[ink::test]
        fn no_collision_with_single_participant() {
            let sat = deploy();
            let accs = accounts();
            let participants = vec![accs.bob];
            let winners = sat.detect_collisions(&participants);
            assert!(winners.is_empty(), "single participant cannot self-collide");
        }

        #[ink::test]
        fn identical_addresses_collide() {
            let sat = deploy();
            let accs = accounts();
            // Two identical addresses will always share the same bucket.
            let participants = vec![accs.bob, accs.bob];
            let winners = sat.detect_collisions(&participants);
            assert_eq!(winners.len(), 2, "identical addresses should collide");
        }

        // ── Stale epoch guard ─────────────────────────────────────────────────

        #[ink::test]
        fn flush_rejects_stale_epoch_id() {
            let mut sat = deploy();
            let accs = accounts();
            set_caller(accs.alice); // owner

            // epoch_id is 0; passing 1 should be rejected.
            let result = sat.flush_epoch(1);
            assert_eq!(result, Err(Error::StaleEpochTrigger));
        }

        // ── Access control ────────────────────────────────────────────────────

        #[ink::test]
        fn flush_rejects_non_owner() {
            let mut sat = deploy();
            let accs = accounts();
            set_caller(accs.bob); // not the owner
            let result = sat.flush_epoch(0);
            assert_eq!(result, Err(Error::NotOwner));
        }

        #[ink::test]
        fn paused_satellite_rejects_registration() {
            let mut sat = deploy();
            let accs = accounts();
            set_caller(accs.alice);
            sat.set_paused(true).unwrap();
            set_caller(accs.bob);
            let result = sat.register_participant(accs.bob);
            assert_eq!(result, Err(Error::ContractPaused));
        }

        // ── Preview buckets ───────────────────────────────────────────────────

        #[ink::test]
        fn preview_buckets_returns_correct_length() {
            let mut sat = deploy();
            let accs = accounts();
            sat.register_participant(accs.bob).unwrap();
            sat.register_participant(accs.django).unwrap();
            let buckets = sat.preview_buckets();
            assert_eq!(buckets.len(), 2);
        }
    }
}
