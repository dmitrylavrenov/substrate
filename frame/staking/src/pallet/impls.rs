// This file is part of Substrate.

// Copyright (C) 2017-2021 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Implementations for the Staking FRAME Pallet.

use frame_election_provider_support::{
	data_provider, ElectionDataProvider, ElectionProvider, SnapshotBounds, SortedListProvider,
	Supports, VoteWeight, VoteWeightProvider,
};
use frame_support::{
	pallet_prelude::*,
	traits::{
		Currency, CurrencyToVote, EstimateNextNewSession, Get, Imbalance, LockableCurrency,
		OnUnbalanced, UnixTime, WithdrawReasons,
	},
	weights::{Weight, WithPostDispatchInfo},
};
use frame_system::pallet_prelude::BlockNumberFor;
use pallet_session::historical;
use sp_runtime::{
	traits::{Bounded, Convert, SaturatedConversion, Saturating, Zero},
	Perbill,
};
use sp_staking::{
	offence::{DisableStrategy, OffenceDetails, OnOffenceHandler},
	SessionIndex,
};
use sp_std::{collections::btree_map::BTreeMap, prelude::*};

use crate::{
	log, slashing, weights::WeightInfo, ActiveEraInfo, BalanceOf, EraIndex, EraPayout, Exposure,
	ExposureOf, Forcing, IndividualExposure, NominationQuota, Nominations, PositiveImbalanceOf,
	RewardDestination, SessionInterface, StakingLedger, ValidatorPrefs,
};

use super::{pallet::*, STAKING_ID};

impl<T: Config> Pallet<T> {
	/// The total balance that can be slashed from a stash account as of right now.
	pub fn slashable_balance_of(stash: &T::AccountId) -> BalanceOf<T> {
		// Weight note: consider making the stake accessible through stash.
		Self::bonded(stash).and_then(Self::ledger).map(|l| l.active).unwrap_or_default()
	}

	/// Internal impl of [`Self::slashable_balance_of`] that returns [`VoteWeight`].
	pub fn slashable_balance_of_vote_weight(
		stash: &T::AccountId,
		issuance: BalanceOf<T>,
	) -> VoteWeight {
		T::CurrencyToVote::to_vote(Self::slashable_balance_of(stash), issuance)
	}

	/// Returns a closure around `slashable_balance_of_vote_weight` that can be passed around.
	///
	/// This prevents call sites from repeatedly requesting `total_issuance` from backend. But it is
	/// important to be only used while the total issuance is not changing.
	pub fn weight_of_fn() -> Box<dyn Fn(&T::AccountId) -> VoteWeight> {
		// NOTE: changing this to unboxed `impl Fn(..)` return type and the pallet will still
		// compile, while some types in mock fail to resolve.
		let issuance = T::Currency::total_issuance();
		Box::new(move |who: &T::AccountId| -> VoteWeight {
			Self::slashable_balance_of_vote_weight(who, issuance)
		})
	}

	/// Same as `weight_of_fn`, but made for one time use.
	pub fn weight_of(who: &T::AccountId) -> VoteWeight {
		let issuance = T::Currency::total_issuance();
		Self::slashable_balance_of_vote_weight(who, issuance)
	}

	pub(super) fn do_payout_stakers(
		validator_stash: T::AccountId,
		era: EraIndex,
	) -> DispatchResultWithPostInfo {
		// Validate input data
		let current_era = CurrentEra::<T>::get().ok_or_else(|| {
			Error::<T>::InvalidEraToReward
				.with_weight(T::WeightInfo::payout_stakers_alive_staked(0))
		})?;
		let history_depth = Self::history_depth();
		ensure!(
			era <= current_era && era >= current_era.saturating_sub(history_depth),
			Error::<T>::InvalidEraToReward
				.with_weight(T::WeightInfo::payout_stakers_alive_staked(0))
		);

		// Note: if era has no reward to be claimed, era may be future. better not to update
		// `ledger.claimed_rewards` in this case.
		let era_payout = <ErasValidatorReward<T>>::get(&era).ok_or_else(|| {
			Error::<T>::InvalidEraToReward
				.with_weight(T::WeightInfo::payout_stakers_alive_staked(0))
		})?;

		let controller = Self::bonded(&validator_stash).ok_or_else(|| {
			Error::<T>::NotStash.with_weight(T::WeightInfo::payout_stakers_alive_staked(0))
		})?;
		let mut ledger = <Ledger<T>>::get(&controller).ok_or(Error::<T>::NotController)?;

		ledger
			.claimed_rewards
			.retain(|&x| x >= current_era.saturating_sub(history_depth));
		match ledger.claimed_rewards.binary_search(&era) {
			Ok(_) => Err(Error::<T>::AlreadyClaimed
				.with_weight(T::WeightInfo::payout_stakers_alive_staked(0)))?,
			Err(pos) => ledger.claimed_rewards.insert(pos, era),
		}

		let exposure = <ErasStakersClipped<T>>::get(&era, &ledger.stash);

		// Input data seems good, no errors allowed after this point

		<Ledger<T>>::insert(&controller, &ledger);

		// Get Era reward points. It has TOTAL and INDIVIDUAL
		// Find the fraction of the era reward that belongs to the validator
		// Take that fraction of the eras rewards to split to nominator and validator
		//
		// Then look at the validator, figure out the proportion of their reward
		// which goes to them and each of their nominators.

		let era_reward_points = <ErasRewardPoints<T>>::get(&era);
		let total_reward_points = era_reward_points.total;
		let validator_reward_points = era_reward_points
			.individual
			.get(&ledger.stash)
			.map(|points| *points)
			.unwrap_or_else(|| Zero::zero());

		// Nothing to do if they have no reward points.
		if validator_reward_points.is_zero() {
			return Ok(Some(T::WeightInfo::payout_stakers_alive_staked(0)).into())
		}

		// This is the fraction of the total reward that the validator and the
		// nominators will get.
		let validator_total_reward_part =
			Perbill::from_rational(validator_reward_points, total_reward_points);

		// This is how much validator + nominators are entitled to.
		let validator_total_payout = validator_total_reward_part * era_payout;

		let validator_prefs = Self::eras_validator_prefs(&era, &validator_stash);
		// Validator first gets a cut off the top.
		let validator_commission = validator_prefs.commission;
		let validator_commission_payout = validator_commission * validator_total_payout;

		let validator_leftover_payout = validator_total_payout - validator_commission_payout;
		// Now let's calculate how this is split to the validator.
		let validator_exposure_part = Perbill::from_rational(exposure.own, exposure.total);
		let validator_staking_payout = validator_exposure_part * validator_leftover_payout;

		Self::deposit_event(Event::<T>::PayoutStarted(era, ledger.stash.clone()));

		// We can now make total validator payout:
		if let Some(imbalance) =
			Self::make_payout(&ledger.stash, validator_staking_payout + validator_commission_payout)
		{
			Self::deposit_event(Event::<T>::Rewarded(ledger.stash, imbalance.peek()));
		}

		// Track the number of payout ops to nominators. Note:
		// `WeightInfo::payout_stakers_alive_staked` always assumes at least a validator is paid
		// out, so we do not need to count their payout op.
		let mut nominator_payout_count: u32 = 0;

		// Lets now calculate how this is split to the nominators.
		// Reward only the clipped exposures. Note this is not necessarily sorted.
		for nominator in exposure.others.iter() {
			let nominator_exposure_part = Perbill::from_rational(nominator.value, exposure.total);

			let nominator_reward: BalanceOf<T> =
				nominator_exposure_part * validator_leftover_payout;
			// We can now make nominator payout:
			if let Some(imbalance) = Self::make_payout(&nominator.who, nominator_reward) {
				// Note: this logic does not count payouts for `RewardDestination::None`.
				nominator_payout_count += 1;
				let e = Event::<T>::Rewarded(nominator.who.clone(), imbalance.peek());
				Self::deposit_event(e);
			}
		}

		debug_assert!(nominator_payout_count <= T::MaxNominatorRewardedPerValidator::get());
		Ok(Some(T::WeightInfo::payout_stakers_alive_staked(nominator_payout_count)).into())
	}

	/// Update the ledger for a controller.
	///
	/// This will also update the stash lock.
	pub(crate) fn update_ledger(
		controller: &T::AccountId,
		ledger: &StakingLedger<T::AccountId, BalanceOf<T>>,
	) {
		T::Currency::set_lock(STAKING_ID, &ledger.stash, ledger.total, WithdrawReasons::all());
		<Ledger<T>>::insert(controller, ledger);
	}

	/// Chill a stash account.
	pub(crate) fn chill_stash(stash: &T::AccountId) {
		let chilled_as_validator = Self::do_remove_validator(stash);
		let chilled_as_nominator = Self::do_remove_nominator(stash);
		if chilled_as_validator || chilled_as_nominator {
			Self::deposit_event(Event::<T>::Chilled(stash.clone()));
		}
	}

	/// Actually make a payment to a staker. This uses the currency's reward function
	/// to pay the right payee for the given staker account.
	fn make_payout(stash: &T::AccountId, amount: BalanceOf<T>) -> Option<PositiveImbalanceOf<T>> {
		let dest = Self::payee(stash);
		match dest {
			RewardDestination::Controller => Self::bonded(stash)
				.and_then(|controller| Some(T::Currency::deposit_creating(&controller, amount))),
			RewardDestination::Stash => T::Currency::deposit_into_existing(stash, amount).ok(),
			RewardDestination::Staked => Self::bonded(stash)
				.and_then(|c| Self::ledger(&c).map(|l| (c, l)))
				.and_then(|(controller, mut l)| {
					l.active += amount;
					l.total += amount;
					let r = T::Currency::deposit_into_existing(stash, amount).ok();
					Self::update_ledger(&controller, &l);
					r
				}),
			RewardDestination::Account(dest_account) =>
				Some(T::Currency::deposit_creating(&dest_account, amount)),
			RewardDestination::None => None,
		}
	}

	/// Plan a new session potentially trigger a new era.
	fn new_session(session_index: SessionIndex, is_genesis: bool) -> Option<Vec<T::AccountId>> {
		if let Some(current_era) = Self::current_era() {
			// Initial era has been set.
			let current_era_start_session_index = Self::eras_start_session_index(current_era)
				.unwrap_or_else(|| {
					frame_support::print("Error: start_session_index must be set for current_era");
					0
				});

			let era_length =
				session_index.checked_sub(current_era_start_session_index).unwrap_or(0); // Must never happen.

			match ForceEra::<T>::get() {
				// Will be set to `NotForcing` again if a new era has been triggered.
				Forcing::ForceNew => (),
				// Short circuit to `try_trigger_new_era`.
				Forcing::ForceAlways => (),
				// Only go to `try_trigger_new_era` if deadline reached.
				Forcing::NotForcing if era_length >= T::SessionsPerEra::get() => (),
				_ => {
					// Either `Forcing::ForceNone`,
					// or `Forcing::NotForcing if era_length >= T::SessionsPerEra::get()`.
					return None
				},
			}

			// New era.
			let maybe_new_era_validators = Self::try_trigger_new_era(session_index, is_genesis);
			if maybe_new_era_validators.is_some() &&
				matches!(ForceEra::<T>::get(), Forcing::ForceNew)
			{
				ForceEra::<T>::put(Forcing::NotForcing);
			}

			maybe_new_era_validators
		} else {
			// Set initial era.
			log!(debug, "Starting the first era.");
			Self::try_trigger_new_era(session_index, is_genesis)
		}
	}

	/// Start a session potentially starting an era.
	fn start_session(start_session: SessionIndex) {
		let next_active_era = Self::active_era().map(|e| e.index + 1).unwrap_or(0);
		// This is only `Some` when current era has already progressed to the next era, while the
		// active era is one behind (i.e. in the *last session of the active era*, or *first session
		// of the new current era*, depending on how you look at it).
		if let Some(next_active_era_start_session_index) =
			Self::eras_start_session_index(next_active_era)
		{
			if next_active_era_start_session_index == start_session {
				Self::start_era(start_session);
			} else if next_active_era_start_session_index < start_session {
				// This arm should never happen, but better handle it than to stall the staking
				// pallet.
				frame_support::print("Warning: A session appears to have been skipped.");
				Self::start_era(start_session);
			}
		}

		// disable all offending validators that have been disabled for the whole era
		for (index, disabled) in <OffendingValidators<T>>::get() {
			if disabled {
				T::SessionInterface::disable_validator(index);
			}
		}
	}

	/// End a session potentially ending an era.
	fn end_session(session_index: SessionIndex) {
		if let Some(active_era) = Self::active_era() {
			if let Some(next_active_era_start_session_index) =
				Self::eras_start_session_index(active_era.index + 1)
			{
				if next_active_era_start_session_index == session_index + 1 {
					Self::end_era(active_era, session_index);
				}
			}
		}
	}

	///
	/// * Increment `active_era.index`,
	/// * reset `active_era.start`,
	/// * update `BondedEras` and apply slashes.
	fn start_era(start_session: SessionIndex) {
		let active_era = ActiveEra::<T>::mutate(|active_era| {
			let new_index = active_era.as_ref().map(|info| info.index + 1).unwrap_or(0);
			*active_era = Some(ActiveEraInfo {
				index: new_index,
				// Set new active era start in next `on_finalize`. To guarantee usage of `Time`
				start: None,
			});
			new_index
		});

		let bonding_duration = T::BondingDuration::get();

		BondedEras::<T>::mutate(|bonded| {
			bonded.push((active_era, start_session));

			if active_era > bonding_duration {
				let first_kept = active_era - bonding_duration;

				// Prune out everything that's from before the first-kept index.
				let n_to_prune =
					bonded.iter().take_while(|&&(era_idx, _)| era_idx < first_kept).count();

				// Kill slashing metadata.
				for (pruned_era, _) in bonded.drain(..n_to_prune) {
					slashing::clear_era_metadata::<T>(pruned_era);
				}

				if let Some(&(_, first_session)) = bonded.first() {
					T::SessionInterface::prune_historical_up_to(first_session);
				}
			}
		});

		Self::apply_unapplied_slashes(active_era);
	}

	/// Compute payout for era.
	fn end_era(active_era: ActiveEraInfo, _session_index: SessionIndex) {
		// Note: active_era_start can be None if end era is called during genesis config.
		if let Some(active_era_start) = active_era.start {
			let now_as_millis_u64 = T::UnixTime::now().as_millis().saturated_into::<u64>();

			let era_duration = (now_as_millis_u64 - active_era_start).saturated_into::<u64>();
			let staked = Self::eras_total_stake(&active_era.index);
			let issuance = T::Currency::total_issuance();
			let (validator_payout, rest) = T::EraPayout::era_payout(staked, issuance, era_duration);

			Self::deposit_event(Event::<T>::EraPaid(active_era.index, validator_payout, rest));

			// Set ending era reward.
			<ErasValidatorReward<T>>::insert(&active_era.index, validator_payout);
			T::RewardRemainder::on_unbalanced(T::Currency::issue(rest));

			// Clear offending validators.
			<OffendingValidators<T>>::kill();
		}
	}

	/// Plan a new era.
	///
	/// * Bump the current era storage (which holds the latest planned era).
	/// * Store start session index for the new planned era.
	/// * Clean old era information.
	/// * Store staking information for the new planned era
	///
	/// Returns the new validator set.
	pub fn trigger_new_era(
		start_session_index: SessionIndex,
		exposures: Vec<(T::AccountId, Exposure<T::AccountId, BalanceOf<T>>)>,
	) -> Vec<T::AccountId> {
		// Increment or set current era.
		let new_planned_era = CurrentEra::<T>::mutate(|s| {
			*s = Some(s.map(|s| s + 1).unwrap_or(0));
			s.unwrap()
		});
		ErasStartSessionIndex::<T>::insert(&new_planned_era, &start_session_index);

		// Clean old era information.
		if let Some(old_era) = new_planned_era.checked_sub(Self::history_depth() + 1) {
			Self::clear_era_information(old_era);
		}

		// Set staking information for the new era.
		Self::store_stakers_info(exposures, new_planned_era)
	}

	/// Potentially plan a new era.
	///
	/// Get election result from `T::ElectionProvider`.
	/// In case election result has more than [`MinimumValidatorCount`] validator trigger a new era.
	///
	/// In case a new era is planned, the new validator set is returned.
	pub(crate) fn try_trigger_new_era(
		start_session_index: SessionIndex,
		is_genesis: bool,
	) -> Option<Vec<T::AccountId>> {
		let election_result = if is_genesis {
			T::GenesisElectionProvider::elect().map_err(|e| {
				log!(warn, "genesis election provider failed due to {:?}", e);
				Self::deposit_event(Event::StakingElectionFailed);
			})
		} else {
			T::ElectionProvider::elect().map_err(|e| {
				log!(warn, "election provider failed due to {:?}", e);
				Self::deposit_event(Event::StakingElectionFailed);
			})
		}
		.ok()?;

		let exposures = Self::collect_exposures(election_result);
		if (exposures.len() as u32) < Self::minimum_validator_count().max(1) {
			// Session will panic if we ever return an empty validator set, thus max(1) ^^.
			match CurrentEra::<T>::get() {
				Some(current_era) if current_era > 0 => log!(
					warn,
					"chain does not have enough staking candidates to operate for era {:?} ({} \
					elected, minimum is {})",
					CurrentEra::<T>::get().unwrap_or(0),
					exposures.len(),
					Self::minimum_validator_count(),
				),
				None => {
					// The initial era is allowed to have no exposures.
					// In this case the SessionManager is expected to choose a sensible validator
					// set.
					// TODO: this should be simplified #8911
					CurrentEra::<T>::put(0);
					ErasStartSessionIndex::<T>::insert(&0, &start_session_index);
				},
				_ => (),
			}

			Self::deposit_event(Event::StakingElectionFailed);
			return None
		}

		Self::deposit_event(Event::StakersElected);
		Some(Self::trigger_new_era(start_session_index, exposures))
	}

	/// Process the output of the election.
	///
	/// Store staking information for the new planned era
	pub fn store_stakers_info(
		exposures: Vec<(T::AccountId, Exposure<T::AccountId, BalanceOf<T>>)>,
		new_planned_era: EraIndex,
	) -> Vec<T::AccountId> {
		let elected_stashes = exposures.iter().cloned().map(|(x, _)| x).collect::<Vec<_>>();

		// Populate stakers, exposures, and the snapshot of validator prefs.
		let mut total_stake: BalanceOf<T> = Zero::zero();
		exposures.into_iter().for_each(|(stash, exposure)| {
			total_stake = total_stake.saturating_add(exposure.total);
			<ErasStakers<T>>::insert(new_planned_era, &stash, &exposure);

			let mut exposure_clipped = exposure;
			let clipped_max_len = T::MaxNominatorRewardedPerValidator::get() as usize;
			if exposure_clipped.others.len() > clipped_max_len {
				exposure_clipped.others.sort_by(|a, b| a.value.cmp(&b.value).reverse());
				exposure_clipped.others.truncate(clipped_max_len);
			}
			<ErasStakersClipped<T>>::insert(&new_planned_era, &stash, exposure_clipped);
		});

		// Insert current era staking information
		<ErasTotalStake<T>>::insert(&new_planned_era, total_stake);

		// Collect the pref of all winners.
		for stash in &elected_stashes {
			let pref = Self::validators(stash);
			<ErasValidatorPrefs<T>>::insert(&new_planned_era, stash, pref);
		}

		if new_planned_era > 0 {
			log!(
				info,
				"new validator set of size {:?} has been processed for era {:?}",
				elected_stashes.len(),
				new_planned_era,
			);
		}

		elected_stashes
	}

	/// Consume a set of [`Supports`] from [`sp_npos_elections`] and collect them into a
	/// [`Exposure`].
	fn collect_exposures(
		supports: Supports<T::AccountId>,
	) -> Vec<(T::AccountId, Exposure<T::AccountId, BalanceOf<T>>)> {
		let total_issuance = T::Currency::total_issuance();
		let to_currency = |e: frame_election_provider_support::ExtendedBalance| {
			T::CurrencyToVote::to_currency(e, total_issuance)
		};

		supports
			.into_iter()
			.map(|(validator, support)| {
				// Build `struct exposure` from `support`.
				let mut others = Vec::with_capacity(support.voters.len());
				let mut own: BalanceOf<T> = Zero::zero();
				let mut total: BalanceOf<T> = Zero::zero();
				support
					.voters
					.into_iter()
					.map(|(nominator, weight)| (nominator, to_currency(weight)))
					.for_each(|(nominator, stake)| {
						if nominator == validator {
							own = own.saturating_add(stake);
						} else {
							others.push(IndividualExposure { who: nominator, value: stake });
						}
						total = total.saturating_add(stake);
					});

				let exposure = Exposure { own, others, total };
				(validator, exposure)
			})
			.collect::<Vec<(T::AccountId, Exposure<_, _>)>>()
	}

	/// Remove all associated data of a stash account from the staking system.
	///
	/// Assumes storage is upgraded before calling.
	///
	/// This is called:
	/// - after a `withdraw_unbonded()` call that frees all of a stash's bonded balance.
	/// - through `reap_stash()` if the balance has fallen to zero (through slashing).
	pub(crate) fn kill_stash(stash: &T::AccountId, num_slashing_spans: u32) -> DispatchResult {
		let controller = <Bonded<T>>::get(stash).ok_or(Error::<T>::NotStash)?;

		slashing::clear_stash_metadata::<T>(stash, num_slashing_spans)?;

		<Bonded<T>>::remove(stash);
		<Ledger<T>>::remove(&controller);

		<Payee<T>>::remove(stash);
		Self::do_remove_validator(stash);
		Self::do_remove_nominator(stash);

		frame_system::Pallet::<T>::dec_consumers(stash);

		Ok(())
	}

	/// Clear all era information for given era.
	pub(crate) fn clear_era_information(era_index: EraIndex) {
		<ErasStakers<T>>::remove_prefix(era_index, None);
		<ErasStakersClipped<T>>::remove_prefix(era_index, None);
		<ErasValidatorPrefs<T>>::remove_prefix(era_index, None);
		<ErasValidatorReward<T>>::remove(era_index);
		<ErasRewardPoints<T>>::remove(era_index);
		<ErasTotalStake<T>>::remove(era_index);
		ErasStartSessionIndex::<T>::remove(era_index);
	}

	/// Apply previously-unapplied slashes on the beginning of a new era, after a delay.
	fn apply_unapplied_slashes(active_era: EraIndex) {
		let slash_defer_duration = T::SlashDeferDuration::get();
		<Self as Store>::EarliestUnappliedSlash::mutate(|earliest| {
			if let Some(ref mut earliest) = earliest {
				let keep_from = active_era.saturating_sub(slash_defer_duration);
				for era in (*earliest)..keep_from {
					let era_slashes = <Self as Store>::UnappliedSlashes::take(&era);
					for slash in era_slashes {
						slashing::apply_slash::<T>(slash);
					}
				}

				*earliest = (*earliest).max(keep_from)
			}
		})
	}

	/// Add reward points to validators using their stash account ID.
	///
	/// Validators are keyed by stash account ID and must be in the current elected set.
	///
	/// For each element in the iterator the given number of points in u32 is added to the
	/// validator, thus duplicates are handled.
	///
	/// At the end of the era each the total payout will be distributed among validator
	/// relatively to their points.
	///
	/// COMPLEXITY: Complexity is `number_of_validator_to_reward x current_elected_len`.
	pub fn reward_by_ids(validators_points: impl IntoIterator<Item = (T::AccountId, u32)>) {
		if let Some(active_era) = Self::active_era() {
			<ErasRewardPoints<T>>::mutate(active_era.index, |era_rewards| {
				for (validator, points) in validators_points.into_iter() {
					*era_rewards.individual.entry(validator).or_default() += points;
					era_rewards.total += points;
				}
			});
		}
	}

	/// Ensures that at the end of the current session there will be a new era.
	pub(crate) fn ensure_new_era() {
		match ForceEra::<T>::get() {
			Forcing::ForceAlways | Forcing::ForceNew => (),
			_ => ForceEra::<T>::put(Forcing::ForceNew),
		}
	}

	#[cfg(feature = "runtime-benchmarks")]
	pub fn add_era_stakers(
		current_era: EraIndex,
		controller: T::AccountId,
		exposure: Exposure<T::AccountId, BalanceOf<T>>,
	) {
		<ErasStakers<T>>::insert(&current_era, &controller, &exposure);
	}

	#[cfg(feature = "runtime-benchmarks")]
	pub fn set_slash_reward_fraction(fraction: Perbill) {
		SlashRewardFraction::<T>::put(fraction);
	}

	/// Get all of the voters that are eligible for the next npos election.
	///
	/// This function is self-weighing as [`DispatchClass::Mandatory`].
	///
	/// ### Slashing
	///
	/// All nominations that have been submitted before the last non-zero slash of the validator are
	/// auto-chilled.
	///
	/// # Warning
	///
	/// This is the unbounded variant. Being called might cause a large number of storage reads. Use
	/// [`get_npos_targets_bounded`] otherwise.
	pub fn get_npos_voters_unbounded() -> Vec<(T::AccountId, VoteWeight, Vec<T::AccountId>)> {
		let slashing_spans = <SlashingSpans<T>>::iter().collect::<BTreeMap<_, _>>();
		let weight_of = Self::weight_of_fn();

		let mut nominator_votes = T::SortedListProvider::iter()
			.filter_map(|nominator| {
				if let Some(Nominations { submitted_in, mut targets, suppressed: _ }) =
					Self::nominators(nominator.clone())
				{
					targets.retain(|validator_stash| {
						slashing_spans
							.get(validator_stash)
							.map_or(true, |spans| submitted_in >= spans.last_nonzero_slash())
					});
					if !targets.is_empty() {
						let weight = weight_of(&nominator);
						return Some((nominator, weight, targets))
					}
				}
				None
			})
			.collect::<Vec<_>>();

		let validator_votes = <Validators<T>>::iter()
			.map(|(v, _)| (v.clone(), Self::weight_of(&v), vec![v.clone()]))
			.collect::<Vec<_>>();

		Self::register_weight(T::WeightInfo::get_npos_voters_unbounded(
			validator_votes.len() as u32,
			nominator_votes.len() as u32,
			slashing_spans.len() as u32,
		));
		log!(
			info,
			"generated {} npos voters, {} from validators and {} nominators, without size limit",
			validator_votes.len() + nominator_votes.len(),
			validator_votes.len(),
			nominator_votes.len(),
		);

		// NOTE: we chain the one we expect to have the smaller size (`validators_votes`) to the
		// larger one, to minimize copying. Ideally we would collect only once, but sadly then we
		// wouldn't have access to a cheap `.len()`, which we need for weighing. Only other option
		// would have been using the counters, but since we entirely avoid reading them, we better
		// stick to that.
		nominator_votes.extend(validator_votes);
		nominator_votes
	}

	/// Get all of the voters that are eligible for the next npos election.
	///
	/// `bounds` imposes a cap on the count and byte-size of the entire vector returned.
	///
	/// As of now, first all the validator are included in no particular order, then remainder is
	/// taken from the nominators, as returned by [`Config::SortedListProvider`].
	///
	/// This function is self-weighing as [`DispatchClass::Mandatory`].
	///
	/// ### Slashing
	///
	/// All nominations that have been submitted before the last non-zero slash of the validator are
	/// auto-chilled, and they **DO** count towards the limit imposed by `bounds`. To prevent this
	/// from getting in the way, [`update_slashed_nominator`] can be used to clean these stale
	/// nominations.
	pub fn get_npos_voters_bounded(
		bounds: SnapshotBounds,
	) -> Vec<(T::AccountId, VoteWeight, Vec<T::AccountId>)> {
		let mut tracker = StaticSizeTracker::<T::AccountId>::new();
		let mut voters = if let Some(capacity) =
			bounds.predict_capacity(StaticSizeTracker::<T::AccountId>::voter_size(
				T::NominationQuota::ABSOLUTE_MAXIMUM as usize,
			)) {
			Vec::with_capacity(capacity.min(sp_core::MAX_POSSIBLE_ALLOCATION as usize / 2))
		} else {
			Vec::new()
		};

		// we create two closures to make us agnostic of the type of `bounds` that we are dealing
		// with. This still not the optimum. The most performant option would have been having a
		// dedicated function for each variant. For example, in the current code, if `bounds`
		// count-bounded, the static size tracker is allocated for now reason. Nonetheless, it is
		// not actually tracking anything if it is not needed.

		// register a voter with `votes` with regards to bounds.
		let add_voter = |tracker_ref: &mut StaticSizeTracker<T::AccountId>, votes: usize| {
			if let Some(_) = bounds.size_bound() {
				tracker_ref.register_voter(votes)
			} else if let Some(_) = bounds.count_bound() {
				// nothing needed, voters.len() is itself a representation of the count.
			}
		};

		// check if adding one more voter will exhaust any of the bounds.
		let next_will_exhaust = |tracker_ref: &StaticSizeTracker<T::AccountId>,
		                         voters_ref: &Vec<_>| match (
			bounds.size_bound(),
			bounds.count_bound(),
		) {
			(Some(max_size), Some(max_count)) =>
				tracker_ref.final_byte_size_of(voters_ref.len().saturating_add(1)) > max_size ||
					voters_ref.len().saturating_add(1) > max_count,
			(Some(max_size), None) =>
				tracker_ref.final_byte_size_of(voters_ref.len().saturating_add(1)) > max_size,
			(None, Some(max_count)) => voters_ref.len().saturating_add(1) > max_count,
			(None, None) => false,
		};

		// first, grab all validators in no particular order. In most cases, all of them should fit
		// anyway.
		for (validator, _) in <Validators<T>>::iter() {
			let self_vote =
				(validator.clone(), Self::weight_of(&validator), vec![validator.clone()]);
			add_voter(&mut tracker, 1);
			if next_will_exhaust(&tracker, &voters) {
				log!(
					warn,
					"stopped iterating over validators' self-vote at {} due to bound {:?}",
					voters.len(),
					bounds,
				);
				break
			}
			voters.push(self_vote);
		}
		let validators_taken = voters.len();

		// only bother with reading the slashing spans et.al. if we are not exhausted.
		let slashing_spans_read = if !next_will_exhaust(&tracker, &voters) {
			let slashing_spans = <SlashingSpans<T>>::iter().collect::<BTreeMap<_, _>>();
			let weight_of = Self::weight_of_fn();

			for nominator in T::SortedListProvider::iter() {
				if let Some(Nominations { submitted_in, mut targets, suppressed: _ }) =
					<Nominators<T>>::get(&nominator)
				{
					// IMPORTANT: we track the size and potentially break out right here. This
					// ensures that votes that are invalid will also affect the snapshot bounds.
					// Chain operators should ensure `update_slashed_nominator` is used to eliminate
					// the need for this.
					add_voter(&mut tracker, targets.len());
					if next_will_exhaust(&tracker, &voters) {
						break
					}

					targets.retain(|stash| {
						slashing_spans
							.get(stash)
							.map_or(true, |spans| submitted_in >= spans.last_nonzero_slash())
					});
					if !targets.len().is_zero() {
						voters.push((nominator.clone(), weight_of(&nominator), targets));
					}
				} else {
					log!(error, "DEFENSIVE: invalid item in `SortedListProvider`: {:?}", nominator)
				}
			}
			slashing_spans.len() as u32
		} else {
			Zero::zero()
		};

		let nominators_taken = voters.len().saturating_sub(validators_taken);
		Self::register_weight(T::WeightInfo::get_npos_voters_bounded(
			validators_taken as u32,
			nominators_taken as u32,
			slashing_spans_read,
		));

		debug_assert!(
			!bounds.exhausts_size_count_non_zero(
				|| voters.encoded_size() as u32,
				|| voters.len() as u32
			),
			"{} voters, size {}, exhausted {:?}",
			voters.len(),
			voters.encoded_size(),
			bounds,
		);

		log!(
			info,
			"generated {} npos voters, {} from validators and {} nominators, with bound {:?}",
			voters.len(),
			validators_taken,
			nominators_taken,
			bounds,
		);
		voters
	}

	/// Get the list of targets (validators) that are eligible for the next npos election.
	///
	/// This function is self-weighing as [`DispatchClass::Mandatory`].
	///
	/// # Warning
	///
	/// This is the unbounded variant. Being called might cause a large number of storage reads. Use
	/// [`get_npos_targets_bounded`] otherwise.
	pub fn get_npos_targets_unbounded() -> Vec<T::AccountId> {
		let targets = Validators::<T>::iter().map(|(v, _)| v).collect::<Vec<_>>();
		Self::register_weight(T::WeightInfo::get_npos_targets_unbounded(targets.len() as u32));
		log!(info, "generated {} npos targets, without size limit.", targets.len());
		targets
	}

	/// Get the list of targets (validators) that are eligible for the next npos election.
	///
	/// `bounds` imposes a cap on the count and byte-size of the entire targets returned.
	///
	/// This function is self-weighing as [`DispatchClass::Mandatory`].
	pub fn get_npos_targets_bounded(bounds: SnapshotBounds) -> Vec<T::AccountId> {
		let mut internal_size: usize = Zero::zero();
		let mut targets: Vec<T::AccountId> = if let Some(capacity) =
			bounds.predict_capacity(sp_std::mem::size_of::<T::AccountId>())
		{
			Vec::with_capacity(capacity.min(sp_core::MAX_POSSIBLE_ALLOCATION as usize / 2))
		} else {
			Vec::new()
		};

		let next_will_exhaust =
			|new_final_size, new_count| match (bounds.size_bound(), bounds.count_bound()) {
				(Some(max_size), Some(max_count)) =>
					new_final_size > max_size || new_count > max_count,
				(Some(max_size), None) => new_final_size > max_size,
				(None, Some(max_count)) => new_count > max_count,
				(None, None) => false,
			};

		for (next, _) in Validators::<T>::iter() {
			// TODO: rather sub-optimal, we should not need to track size if it is not bounded.
			let new_internal_size = internal_size + sp_std::mem::size_of::<T::AccountId>();
			let new_final_size = new_internal_size +
				StaticSizeTracker::<T::AccountId>::length_prefix(targets.len() + 1);
			let new_count = targets.len() + 1;
			if next_will_exhaust(new_final_size, new_count) {
				// we've had enough
				break
			}
			targets.push(next);
			internal_size = new_internal_size;

			debug_assert_eq!(targets.encoded_size(), new_final_size);
		}

		Self::register_weight(T::WeightInfo::get_npos_targets_bounded(targets.len() as u32));
		debug_assert!(!bounds.exhausts_size_count_non_zero(
			|| targets.encoded_size() as u32,
			|| targets.len() as u32
		));

		log!(info, "generated {} npos targets, with bounds limit {:?}", targets.len(), bounds);
		targets
	}

	/// This function will add a nominator to the `Nominators` storage map,
	/// [`SortedListProvider`] and keep track of the `CounterForNominators`.
	///
	/// If the nominator already exists, their nominations will be updated.
	///
	/// NOTE: you must ALWAYS use this function to add nominator or update their targets. Any access
	/// to `Nominators`, its counter, or `VoterList` outside of this function is almost certainly
	/// wrong.
	pub fn do_add_nominator(who: &T::AccountId, nominations: Nominations<T::AccountId>) {
		if !Nominators::<T>::contains_key(who) {
			// maybe update the counter.
			CounterForNominators::<T>::mutate(|x| x.saturating_inc());

			// maybe update sorted list. Error checking is defensive-only - this should never fail.
			if T::SortedListProvider::on_insert(who.clone(), Self::weight_of(who)).is_err() {
				log!(warn, "attempt to insert duplicate nominator ({:#?})", who);
				debug_assert!(false, "attempt to insert duplicate nominator");
			};

			debug_assert_eq!(T::SortedListProvider::sanity_check(), Ok(()));
		}

		Nominators::<T>::insert(who, nominations);
	}

	/// This function will remove a nominator from the `Nominators` storage map,
	/// [`SortedListProvider`] and keep track of the `CounterForNominators`.
	///
	/// Returns true if `who` was removed from `Nominators`, otherwise false.
	///
	/// NOTE: you must ALWAYS use this function to remove a nominator from the system. Any access to
	/// `Nominators`, its counter, or `VoterList` outside of this function is almost certainly
	/// wrong.
	pub fn do_remove_nominator(who: &T::AccountId) -> bool {
		if Nominators::<T>::contains_key(who) {
			Nominators::<T>::remove(who);
			CounterForNominators::<T>::mutate(|x| x.saturating_dec());
			T::SortedListProvider::on_remove(who);
			debug_assert_eq!(T::SortedListProvider::sanity_check(), Ok(()));
			debug_assert_eq!(CounterForNominators::<T>::get(), T::SortedListProvider::count());
			true
		} else {
			false
		}
	}

	/// This function will add a validator to the `Validators` storage map, and keep track of the
	/// `CounterForValidators`.
	///
	/// If the validator already exists, their preferences will be updated.
	///
	/// NOTE: you must ALWAYS use this function to add a validator to the system. Any access to
	/// `Validators`, its counter, or `VoterList` outside of this function is almost certainly
	/// wrong.
	pub fn do_add_validator(who: &T::AccountId, prefs: ValidatorPrefs) {
		if !Validators::<T>::contains_key(who) {
			CounterForValidators::<T>::mutate(|x| x.saturating_inc())
		}
		Validators::<T>::insert(who, prefs);
	}

	/// This function will remove a validator from the `Validators` storage map,
	/// and keep track of the `CounterForValidators`.
	///
	/// Returns true if `who` was removed from `Validators`, otherwise false.
	///
	/// NOTE: you must ALWAYS use this function to remove a validator from the system. Any access to
	/// `Validators`, its counter, or `VoterList` outside of this function is almost certainly
	/// wrong.
	pub fn do_remove_validator(who: &T::AccountId) -> bool {
		if Validators::<T>::contains_key(who) {
			Validators::<T>::remove(who);
			CounterForValidators::<T>::mutate(|x| x.saturating_dec());
			true
		} else {
			false
		}
	}

	/// Register some amount of weight directly with the system pallet.
	///
	/// This is always mandatory weight.
	fn register_weight(weight: Weight) {
		<frame_system::Pallet<T>>::register_extra_weight_unchecked(
			weight,
			DispatchClass::Mandatory,
		);
	}
}

impl<T: Config> ElectionDataProvider<T::AccountId, BlockNumberFor<T>> for Pallet<T> {
	const MAXIMUM_VOTES_PER_VOTER: u32 = T::NominationQuota::ABSOLUTE_MAXIMUM;

	fn desired_targets() -> data_provider::Result<u32> {
		Self::register_weight(T::DbWeight::get().reads(1));
		Ok(Self::validator_count())
	}

	fn voters(
		bounds: SnapshotBounds,
	) -> data_provider::Result<Vec<(T::AccountId, VoteWeight, Vec<T::AccountId>)>> {
		Ok(if bounds.is_unbounded() {
			log!(
				warn,
				"iterating over an unbounded number of npos voters, this might exhaust the \
				memory limits of the chain. Ensure proper limits are set via \
				`MaxNominatorsCount` or `ElectionProvider`"
			);
			Self::get_npos_voters_unbounded()
		} else {
			Self::get_npos_voters_bounded(bounds)
		})
	}

	fn targets(bounds: SnapshotBounds) -> data_provider::Result<Vec<T::AccountId>> {
		Ok(if bounds.is_unbounded() {
			log!(
				warn,
				"iterating over an unbounded number of npos targets, this might exhaust the \
					memory limits of the chain. Ensure proper limits are set via \
					`MaxValidatorsCount` or `ElectionProvider`"
			);
			Self::get_npos_targets_unbounded()
		} else {
			Self::get_npos_targets_bounded(bounds)
		})
	}

	fn next_election_prediction(now: T::BlockNumber) -> T::BlockNumber {
		let current_era = Self::current_era().unwrap_or(0);
		let current_session = Self::current_planned_session();
		let current_era_start_session_index =
			Self::eras_start_session_index(current_era).unwrap_or(0);
		// Number of session in the current era or the maximum session per era if reached.
		let era_progress = current_session
			.saturating_sub(current_era_start_session_index)
			.min(T::SessionsPerEra::get());

		let until_this_session_end = T::NextNewSession::estimate_next_new_session(now)
			.0
			.unwrap_or_default()
			.saturating_sub(now);

		let session_length = T::NextNewSession::average_session_length();

		let sessions_left: T::BlockNumber = match ForceEra::<T>::get() {
			Forcing::ForceNone => Bounded::max_value(),
			Forcing::ForceNew | Forcing::ForceAlways => Zero::zero(),
			Forcing::NotForcing if era_progress >= T::SessionsPerEra::get() => Zero::zero(),
			Forcing::NotForcing => T::SessionsPerEra::get()
				.saturating_sub(era_progress)
				// One session is computed in this_session_end.
				.saturating_sub(1)
				.into(),
		};

		now.saturating_add(
			until_this_session_end.saturating_add(sessions_left.saturating_mul(session_length)),
		)
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn add_voter(voter: T::AccountId, weight: VoteWeight, targets: Vec<T::AccountId>) {
		let stake = <BalanceOf<T>>::try_from(weight).unwrap_or_else(|_| {
			panic!("cannot convert a VoteWeight into BalanceOf, benchmark needs reconfiguring.")
		});
		<Bonded<T>>::insert(voter.clone(), voter.clone());
		<Ledger<T>>::insert(
			voter.clone(),
			StakingLedger {
				stash: voter.clone(),
				active: stake,
				total: stake,
				unlocking: vec![],
				claimed_rewards: vec![],
			},
		);
		Self::do_add_nominator(&voter, Nominations { targets, submitted_in: 0, suppressed: false });
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn add_target(target: T::AccountId) {
		let stake = MinValidatorBond::<T>::get() * 100u32.into();
		<Bonded<T>>::insert(target.clone(), target.clone());
		<Ledger<T>>::insert(
			target.clone(),
			StakingLedger {
				stash: target.clone(),
				active: stake,
				total: stake,
				unlocking: vec![],
				claimed_rewards: vec![],
			},
		);
		Self::do_add_validator(
			&target,
			ValidatorPrefs { commission: Perbill::zero(), blocked: false },
		);
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn clear() {
		<Bonded<T>>::remove_all(None);
		<Ledger<T>>::remove_all(None);
		<Validators<T>>::remove_all(None);
		<Nominators<T>>::remove_all(None);
		<CounterForNominators<T>>::kill();
		<CounterForValidators<T>>::kill();
		let _ = T::SortedListProvider::clear(None);
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn put_snapshot(
		voters: Vec<(T::AccountId, VoteWeight, Vec<T::AccountId>)>,
		targets: Vec<T::AccountId>,
		target_stake: Option<VoteWeight>,
	) {
		targets.into_iter().for_each(|v| {
			let stake: BalanceOf<T> = target_stake
				.and_then(|w| <BalanceOf<T>>::try_from(w).ok())
				.unwrap_or_else(|| MinNominatorBond::<T>::get() * 100u32.into());
			<Bonded<T>>::insert(v.clone(), v.clone());
			<Ledger<T>>::insert(
				v.clone(),
				StakingLedger {
					stash: v.clone(),
					active: stake,
					total: stake,
					unlocking: vec![],
					claimed_rewards: vec![],
				},
			);
			Self::do_add_validator(
				&v,
				ValidatorPrefs { commission: Perbill::zero(), blocked: false },
			);
		});

		voters.into_iter().for_each(|(v, s, t)| {
			let stake = <BalanceOf<T>>::try_from(s).unwrap_or_else(|_| {
				panic!("cannot convert a VoteWeight into BalanceOf, benchmark needs reconfiguring.")
			});
			<Bonded<T>>::insert(v.clone(), v.clone());
			<Ledger<T>>::insert(
				v.clone(),
				StakingLedger {
					stash: v.clone(),
					active: stake,
					total: stake,
					unlocking: vec![],
					claimed_rewards: vec![],
				},
			);
			Self::do_add_nominator(
				&v,
				Nominations { targets: t, submitted_in: 0, suppressed: false },
			);
		});
	}
}

/// In this implementation `new_session(session)` must be called before `end_session(session-1)`
/// i.e. the new session must be planned before the ending of the previous session.
///
/// Once the first new_session is planned, all session must start and then end in order, though
/// some session can lag in between the newest session planned and the latest session started.
impl<T: Config> pallet_session::SessionManager<T::AccountId> for Pallet<T> {
	fn new_session(new_index: SessionIndex) -> Option<Vec<T::AccountId>> {
		log!(trace, "planning new session {}", new_index);
		CurrentPlannedSession::<T>::put(new_index);
		Self::new_session(new_index, false)
	}
	fn new_session_genesis(new_index: SessionIndex) -> Option<Vec<T::AccountId>> {
		log!(trace, "planning new session {} at genesis", new_index);
		CurrentPlannedSession::<T>::put(new_index);
		Self::new_session(new_index, true)
	}
	fn start_session(start_index: SessionIndex) {
		log!(trace, "starting session {}", start_index);
		Self::start_session(start_index)
	}
	fn end_session(end_index: SessionIndex) {
		log!(trace, "ending session {}", end_index);
		Self::end_session(end_index)
	}
}

impl<T: Config> historical::SessionManager<T::AccountId, Exposure<T::AccountId, BalanceOf<T>>>
	for Pallet<T>
{
	fn new_session(
		new_index: SessionIndex,
	) -> Option<Vec<(T::AccountId, Exposure<T::AccountId, BalanceOf<T>>)>> {
		<Self as pallet_session::SessionManager<_>>::new_session(new_index).map(|validators| {
			let current_era = Self::current_era()
				// Must be some as a new era has been created.
				.unwrap_or(0);

			validators
				.into_iter()
				.map(|v| {
					let exposure = Self::eras_stakers(current_era, &v);
					(v, exposure)
				})
				.collect()
		})
	}
	fn new_session_genesis(
		new_index: SessionIndex,
	) -> Option<Vec<(T::AccountId, Exposure<T::AccountId, BalanceOf<T>>)>> {
		<Self as pallet_session::SessionManager<_>>::new_session_genesis(new_index).map(
			|validators| {
				let current_era = Self::current_era()
					// Must be some as a new era has been created.
					.unwrap_or(0);

				validators
					.into_iter()
					.map(|v| {
						let exposure = Self::eras_stakers(current_era, &v);
						(v, exposure)
					})
					.collect()
			},
		)
	}
	fn start_session(start_index: SessionIndex) {
		<Self as pallet_session::SessionManager<_>>::start_session(start_index)
	}
	fn end_session(end_index: SessionIndex) {
		<Self as pallet_session::SessionManager<_>>::end_session(end_index)
	}
}

/// Add reward points to block authors:
/// * 20 points to the block producer for producing a (non-uncle) block in the relay chain,
/// * 2 points to the block producer for each reference to a previously unreferenced uncle, and
/// * 1 point to the producer of each referenced uncle block.
impl<T> pallet_authorship::EventHandler<T::AccountId, T::BlockNumber> for Pallet<T>
where
	T: Config + pallet_authorship::Config + pallet_session::Config,
{
	fn note_author(author: T::AccountId) {
		Self::reward_by_ids(vec![(author, 20)])
	}
	fn note_uncle(author: T::AccountId, _age: T::BlockNumber) {
		Self::reward_by_ids(vec![(<pallet_authorship::Pallet<T>>::author(), 2), (author, 1)])
	}
}

/// This is intended to be used with `FilterHistoricalOffences`.
impl<T: Config>
	OnOffenceHandler<T::AccountId, pallet_session::historical::IdentificationTuple<T>, Weight>
	for Pallet<T>
where
	T: pallet_session::Config<ValidatorId = <T as frame_system::Config>::AccountId>,
	T: pallet_session::historical::Config<
		FullIdentification = Exposure<<T as frame_system::Config>::AccountId, BalanceOf<T>>,
		FullIdentificationOf = ExposureOf<T>,
	>,
	T::SessionHandler: pallet_session::SessionHandler<<T as frame_system::Config>::AccountId>,
	T::SessionManager: pallet_session::SessionManager<<T as frame_system::Config>::AccountId>,
	T::ValidatorIdOf: Convert<
		<T as frame_system::Config>::AccountId,
		Option<<T as frame_system::Config>::AccountId>,
	>,
{
	fn on_offence(
		offenders: &[OffenceDetails<
			T::AccountId,
			pallet_session::historical::IdentificationTuple<T>,
		>],
		slash_fraction: &[Perbill],
		slash_session: SessionIndex,
		disable_strategy: DisableStrategy,
	) -> Weight {
		let reward_proportion = SlashRewardFraction::<T>::get();
		let mut consumed_weight: Weight = 0;
		let mut add_db_reads_writes = |reads, writes| {
			consumed_weight += T::DbWeight::get().reads_writes(reads, writes);
		};

		let active_era = {
			let active_era = Self::active_era();
			add_db_reads_writes(1, 0);
			if active_era.is_none() {
				// This offence need not be re-submitted.
				return consumed_weight
			}
			active_era.expect("value checked not to be `None`; qed").index
		};
		let active_era_start_session_index = Self::eras_start_session_index(active_era)
			.unwrap_or_else(|| {
				frame_support::print("Error: start_session_index must be set for current_era");
				0
			});
		add_db_reads_writes(1, 0);

		let window_start = active_era.saturating_sub(T::BondingDuration::get());

		// Fast path for active-era report - most likely.
		// `slash_session` cannot be in a future active era. It must be in `active_era` or before.
		let slash_era = if slash_session >= active_era_start_session_index {
			active_era
		} else {
			let eras = BondedEras::<T>::get();
			add_db_reads_writes(1, 0);

			// Reverse because it's more likely to find reports from recent eras.
			match eras.iter().rev().filter(|&&(_, ref sesh)| sesh <= &slash_session).next() {
				Some(&(ref slash_era, _)) => *slash_era,
				// Before bonding period. defensive - should be filtered out.
				None => return consumed_weight,
			}
		};

		<Self as Store>::EarliestUnappliedSlash::mutate(|earliest| {
			if earliest.is_none() {
				*earliest = Some(active_era)
			}
		});
		add_db_reads_writes(1, 1);

		let slash_defer_duration = T::SlashDeferDuration::get();

		let invulnerables = Self::invulnerables();
		add_db_reads_writes(1, 0);

		for (details, slash_fraction) in offenders.iter().zip(slash_fraction) {
			let (stash, exposure) = &details.offender;

			// Skip if the validator is invulnerable.
			if invulnerables.contains(stash) {
				continue
			}

			let unapplied = slashing::compute_slash::<T>(slashing::SlashParams {
				stash,
				slash: *slash_fraction,
				exposure,
				slash_era,
				window_start,
				now: active_era,
				reward_proportion,
				disable_strategy,
			});

			if let Some(mut unapplied) = unapplied {
				let nominators_len = unapplied.others.len() as u64;
				let reporters_len = details.reporters.len() as u64;

				{
					let upper_bound = 1 /* Validator/NominatorSlashInEra */ + 2 /* fetch_spans */;
					let rw = upper_bound + nominators_len * upper_bound;
					add_db_reads_writes(rw, rw);
				}
				unapplied.reporters = details.reporters.clone();
				if slash_defer_duration == 0 {
					// Apply right away.
					slashing::apply_slash::<T>(unapplied);
					{
						let slash_cost = (6, 5);
						let reward_cost = (2, 2);
						add_db_reads_writes(
							(1 + nominators_len) * slash_cost.0 + reward_cost.0 * reporters_len,
							(1 + nominators_len) * slash_cost.1 + reward_cost.1 * reporters_len,
						);
					}
				} else {
					// Defer to end of some `slash_defer_duration` from now.
					<Self as Store>::UnappliedSlashes::mutate(active_era, move |for_later| {
						for_later.push(unapplied)
					});
					add_db_reads_writes(1, 1);
				}
			} else {
				add_db_reads_writes(4 /* fetch_spans */, 5 /* kick_out_if_recent */)
			}
		}

		consumed_weight
	}
}

impl<T: Config> VoteWeightProvider<T::AccountId> for Pallet<T> {
	fn vote_weight(who: &T::AccountId) -> VoteWeight {
		Self::weight_of(who)
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn set_vote_weight_of(who: &T::AccountId, weight: VoteWeight) {
		// this will clearly results in an inconsistent state, but it should not matter for a
		// benchmark.
		let active: BalanceOf<T> = weight.try_into().map_err(|_| ()).unwrap();
		let mut ledger = Self::ledger(who).unwrap_or_default();
		ledger.active = active;
		<Ledger<T>>::insert(who, ledger);
		<Bonded<T>>::insert(who, who);

		// also, we play a trick to make sure that a issuance based-`CurrencyToVote` behaves well:
		// This will make sure that total issuance is zero, thus the currency to vote will be a 1-1
		// conversion.
		let imbalance = T::Currency::burn(T::Currency::total_issuance());
		// kinda ugly, but gets the job done. The fact that this works here is a HUGE exception.
		// Don't try this pattern in other places.
		sp_std::mem::forget(imbalance);
	}
}

/// A simple voter list implementation that does not require any additional pallets. Note, this
/// does not provided nominators in sorted ordered. If you desire nominators in a sorted order take
/// a look at [`pallet-bags-list].
pub struct UseNominatorsMap<T>(sp_std::marker::PhantomData<T>);
impl<T: Config> SortedListProvider<T::AccountId> for UseNominatorsMap<T> {
	type Error = ();

	/// Returns iterator over voter list, which can have `take` called on it.
	fn iter() -> Box<dyn Iterator<Item = T::AccountId>> {
		Box::new(Nominators::<T>::iter().map(|(n, _)| n))
	}
	fn count() -> u32 {
		CounterForNominators::<T>::get()
	}
	fn contains(id: &T::AccountId) -> bool {
		Nominators::<T>::contains_key(id)
	}
	fn on_insert(_: T::AccountId, _weight: VoteWeight) -> Result<(), Self::Error> {
		// nothing to do on insert.
		Ok(())
	}
	fn on_update(_: &T::AccountId, _weight: VoteWeight) {
		// nothing to do on update.
	}
	fn on_remove(_: &T::AccountId) {
		// nothing to do on remove.
	}
	fn regenerate(
		_: impl IntoIterator<Item = T::AccountId>,
		_: Box<dyn Fn(&T::AccountId) -> VoteWeight>,
	) -> u32 {
		// nothing to do upon regenerate.
		0
	}
	fn sanity_check() -> Result<(), &'static str> {
		Ok(())
	}
	fn clear(maybe_count: Option<u32>) -> u32 {
		Nominators::<T>::remove_all(maybe_count);
		if let Some(count) = maybe_count {
			CounterForNominators::<T>::mutate(|noms| *noms - count);
			count
		} else {
			CounterForNominators::<T>::take()
		}
	}
}

/// A static tracker for the snapshot of all voters.
///
/// Computes the (SCALE) encoded byte length of a snapshot based on static rules, without any actual
/// encoding.
///
/// ## Warning
///
/// Make sure any change to SCALE is reflected here.
///
/// ## Details
///
/// The snapshot has a the form `Vec<Voter>` where `Voter = (Account, u64, Vec<Account>)`. For each
/// voter added to the snapshot, [`register_voter`] should be called, with the number of votes
/// (length of the internal `Vec`).
///
/// Whilst doing this, [`size`] will track the entire size of the `Vec<Voter>`, except for the
/// length prefix of the outer `Vec`. To get the final size at any point, use
/// [`final_byte_size_of`].
pub(crate) struct StaticSizeTracker<AccountId> {
	size: usize,
	_marker: sp_std::marker::PhantomData<AccountId>,
}

impl<AccountId> StaticSizeTracker<AccountId> {
	fn new() -> Self {
		Self { size: 0, _marker: Default::default() }
	}

	/// The length prefix of a vector with the given length.
	#[inline]
	pub(crate) fn length_prefix(length: usize) -> usize {
		// TODO: scale codec could and should expose a public function for this that I can reuse.
		match length {
			0..=63 => 1,
			64..=16383 => 2,
			16384..=1073741823 => 4,
			// this arm almost always never happens. Although, it would be good to get rid of of it,
			// for otherwise we could make this function const, which might enable further
			// optimizations.
			x @ _ => codec::Compact(x as u32).encoded_size(),
		}
	}

	/// Register a voter in `self` who has casted `votes`.
	pub(crate) fn register_voter(&mut self, votes: usize) {
		self.size = self.size.saturating_add(Self::voter_size(votes))
	}

	/// The byte size of a voter who casted `votes`.
	pub(crate) fn voter_size(votes: usize) -> usize {
		Self::length_prefix(votes)
			// and each element
			.saturating_add(votes * sp_std::mem::size_of::<AccountId>())
			// 1 vote-weight
			.saturating_add(sp_std::mem::size_of::<VoteWeight>())
			// 1 voter account
			.saturating_add(sp_std::mem::size_of::<AccountId>())
	}

	// Final size: size of all internal elements, plus the length prefix.
	pub(crate) fn final_byte_size_of(&self, length: usize) -> usize {
		self.size + Self::length_prefix(length)
	}
}

#[cfg(test)]
mod static_tracker {
	use codec::Encode;

	use super::StaticSizeTracker;

	#[test]
	fn len_prefix_works() {
		let length_samples =
			vec![0usize, 1, 62, 63, 64, 16383, 16384, 16385, 1073741822, 1073741823, 1073741824];

		for s in length_samples {
			// the encoded size of a vector of n bytes should be n + the length prefix
			assert_eq!(vec![1u8; s].encoded_size(), StaticSizeTracker::<u64>::length_prefix(s) + s);
		}
	}

	#[test]
	fn length_tracking_works() {
		let mut voters: Vec<(u64, u64, Vec<u64>)> = vec![];
		let mut tracker = StaticSizeTracker::<u64>::new();

		// initial state.
		assert_eq!(voters.encoded_size(), tracker.final_byte_size_of(voters.len()));

		// add a bunch of stuff.
		voters.push((1, 10, vec![1, 2, 3]));
		tracker.register_voter(3);
		assert_eq!(voters.encoded_size(), tracker.final_byte_size_of(voters.len()));

		voters.push((2, 20, vec![1, 3]));
		tracker.register_voter(2);
		assert_eq!(voters.encoded_size(), tracker.final_byte_size_of(voters.len()));

		voters.push((3, 30, vec![1]));
		tracker.register_voter(1);
		assert_eq!(voters.encoded_size(), tracker.final_byte_size_of(voters.len()));

		// unlikely to happen in reality, but anyways.
		voters.push((4, 40, vec![]));
		tracker.register_voter(0);
		assert_eq!(voters.encoded_size(), tracker.final_byte_size_of(voters.len()));
	}
}

/// A helper function that does nothing other than return some information about the range at which
/// the given `bounds` works.
///
/// Will print and return as a tuple as `(low, mid, high)`, where:
///
/// - `low` is the minimum number of voters that `bounds` supports. This will be realized when all
///   voters use [`T::NominationQuota::ABSOLUTE_MAXIMUM`] votes.
/// - `how` is the maximum number of voters that `bounds` supports. This will be realized when all
///   voters use `1` votes.
/// - `mid` is the the average of the above two. This will be realized when all voters use
///   `[`T::NominationQuota::ABSOLUTE_MAXIMUM`] / 2` votes.
#[cfg(feature = "std")]
pub fn display_bounds_limits<T: Config>(bounds: SnapshotBounds) -> (usize, usize, usize) {
	match (bounds.size_bound(), bounds.count_bound()) {
		(None, None) => {
			println!("{:?} is unbounded", bounds);
			(Bounded::max_value(), Bounded::max_value(), Bounded::max_value())
		},
		(None, Some(count)) => {
			println!("{:?} can have exactly maximum {} voters", bounds, count);
			(count, count, count)
		},
		(Some(size), Some(count)) => {
			// maximum number of voters, it means that they all voted 1.
			let max_voters = {
				let voter_size = StaticSizeTracker::<T::AccountId>::voter_size(1);
				// assuming that the length prefix is 4 bytes.
				(size.saturating_sub(4) / voter_size).min(count)
			};
			// minimum number of voters, it means that they all voted maximum.
			let min_voters = {
				let voter_size = StaticSizeTracker::<T::AccountId>::voter_size(
					T::NominationQuota::ABSOLUTE_MAXIMUM as usize,
				);
				(size.saturating_sub(4) / voter_size).min(count)
			};
			// average of the above two.
			let average_voters = {
				let voter_size = StaticSizeTracker::<T::AccountId>::voter_size(
					T::NominationQuota::ABSOLUTE_MAXIMUM as usize / 2,
				);
				// assuming that the length prefix is 4 bytes.
				(size.saturating_sub(4) / voter_size).min(count)
			};
			println!(
				"{:?} will be in [low, mid, high]: [{}, {}, {}]",
				bounds, min_voters, average_voters, max_voters
			);
			(min_voters, average_voters, max_voters)
		},
		(Some(size), None) => {
			// maximum number of voters, it means that they all voted 1.
			let max_voters = {
				let voter_size = StaticSizeTracker::<T::AccountId>::voter_size(1);
				// assuming that the length prefix is 4 bytes.
				size.saturating_sub(4) / voter_size
			};
			// minimum number of voters, it means that they all voted maximum.
			let min_voters = {
				let voter_size = StaticSizeTracker::<T::AccountId>::voter_size(
					T::NominationQuota::ABSOLUTE_MAXIMUM as usize,
				);
				size.saturating_sub(4) / voter_size
			};
			// average of the above two.
			let average_voters = {
				let voter_size = StaticSizeTracker::<T::AccountId>::voter_size(
					T::NominationQuota::ABSOLUTE_MAXIMUM as usize / 2,
				);
				// assuming that the length prefix is 4 bytes.
				size.saturating_sub(4) / voter_size
			};
			println!(
				"{:?} will be in [low, mid, high]: [{}, {}, {}]",
				bounds, min_voters, average_voters, max_voters
			);
			(min_voters, average_voters, max_voters)
		},
	}
}
