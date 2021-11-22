// This file is part of Substrate.

// Copyright (C) 2020 Parity Technologies (UK) Ltd.
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

//! Primitive traits for providing election functionality.
//!
//! This crate provides two traits that could interact to enable extensible election functionality
//! within FRAME pallets.
//!
//! Something that will provide the functionality of election will implement [`ElectionProvider`],
//! whilst needing an associated [`ElectionProvider::DataProvider`], which needs to be fulfilled by
//! an entity implementing [`ElectionDataProvider`]. Most often, *the data provider is* the receiver
//! of the election, resulting in a diagram as below:
//!
//! ```ignore
//!                                         ElectionDataProvider
//!                          <------------------------------------------+
//!                          |                                          |
//!                          v                                          |
//!                    +-----+----+                              +------+---+
//!                    |          |                              |          |
//! pallet-do-election |          |                              |          | pallet-needs-election
//!                    |          |                              |          |
//!                    |          |                              |          |
//!                    +-----+----+                              +------+---+
//!                          |                                          ^
//!                          |                                          |
//!                          +------------------------------------------+
//!                                         ElectionProvider
//! ```
//!
//! > It could also be possible that a third party pallet (C), provides the data of election to an
//! > election provider (B), which then passes the election result to another pallet (A).
//!
//! ## Election Types
//!
//! Typically, two types of elections exist:
//!
//! 1. **Stateless**: Election data is provided, and the election result is immediately ready.
//! 2. **Stateful**: Election data is is queried ahead of time, and the election result might be
//!    ready some number of blocks in the future.
//!
//! To accommodate both type of elections in one trait, the traits lean toward **stateful
//! election**, as it is more general than the stateless. This is why [`ElectionProvider::elect`]
//! has no parameters. All value and type parameter must be provided by the [`ElectionDataProvider`]
//! trait, even if the election happens immediately.
//!
//! ## Election Data
//!
//! The data associated with an election, essentially what the [`ElectionDataProvider`] must convey
//! is as follows:
//!
//! 1. A list of voters, with their stake.
//! 2. A list of targets (i.e. _candidates_).
//! 3. A number of desired targets to be elected (i.e. _winners_)
//!
//! In addition to that, the [`ElectionDataProvider`] must also hint [`ElectionProvider`] at when
//! the next election might happen ([`ElectionDataProvider::next_election_prediction`]). A stateless
//! election provider would probably ignore this. A stateful election provider can use this to
//! prepare the election result in advance.
//!
//! Nonetheless, an [`ElectionProvider`] shan't rely on this and should preferably provide some
//! means of fallback election as well, in case the `elect` was called immaturely early.
//!
//! ## Example
//!
//! ```rust
//! # use frame_election_provider_support::{*, data_provider};
//! # use sp_npos_elections::{Support, Assignment};
//!
//! type AccountId = u64;
//! type Balance = u64;
//! type BlockNumber = u32;
//!
//! mod data_provider_mod {
//!     use super::*;
//!
//!     pub trait Config: Sized {
//!         type ElectionProvider: ElectionProvider<
//!             AccountId,
//!             BlockNumber,
//!             DataProvider = Module<Self>,
//!         >;
//!     }
//!
//!     pub struct Module<T: Config>(std::marker::PhantomData<T>);
//!
//!     impl<T: Config> ElectionDataProvider<AccountId, BlockNumber> for Module<T> {
//!         const MAXIMUM_VOTES_PER_VOTER: u32 = 1;
//!         fn desired_targets() -> data_provider::Result<u32> {
//!             Ok(1)
//!         }
//!         fn voters(_bounds: SnapshotBounds)
//!         -> data_provider::Result<Vec<(AccountId, VoteWeight, Vec<AccountId>)>>
//!         {
//!             Ok(Default::default())
//!         }
//!         fn targets(_bounds: SnapshotBounds) -> data_provider::Result<Vec<AccountId>> {
//!             Ok(vec![10, 20, 30])
//!         }
//!         fn next_election_prediction(now: BlockNumber) -> BlockNumber {
//!             0
//!         }
//!     }
//! }
//!
//!
//! mod generic_election_provider {
//!     use super::*;
//!
//!     pub struct GenericElectionProvider<T: Config>(std::marker::PhantomData<T>);
//!
//!     pub trait Config {
//!         type DataProvider: ElectionDataProvider<AccountId, BlockNumber>;
//!     }
//!
//!     impl<T: Config> ElectionProvider<AccountId, BlockNumber> for GenericElectionProvider<T> {
//!         type Error = &'static str;
//!         type DataProvider = T::DataProvider;
//!
//!         fn elect() -> Result<Supports<AccountId>, Self::Error> {
//!             Self::DataProvider::targets(SnapshotBounds::new_unbounded())
//!                 .map_err(|_| "failed to elect")
//!                 .map(|t| vec![(t[0], Support::default())])
//!         }
//!     }
//! }
//!
//! mod runtime {
//!     use super::generic_election_provider;
//!     use super::data_provider_mod;
//!     use super::AccountId;
//!
//!     struct Runtime;
//!     impl generic_election_provider::Config for Runtime {
//!         type DataProvider = data_provider_mod::Module<Runtime>;
//!     }
//!
//!     impl data_provider_mod::Config for Runtime {
//!         type ElectionProvider = generic_election_provider::GenericElectionProvider<Runtime>;
//!     }
//!
//! }
//!
//! # fn main() {}
//! ```

#![cfg_attr(not(feature = "std"), no_std)]

pub mod onchain;

use codec::{Decode, Encode};
use frame_support::{traits::Get, RuntimeDebug};
use sp_std::{fmt::Debug, prelude::*};

/// Re-export some type as they are used in the interface.
pub use sp_arithmetic::PerThing;
pub use sp_npos_elections::{
	Assignment, ElectionResult, ExtendedBalance, IdentifierT, PerThing128, Support, Supports,
	VoteWeight,
};

/// Types that are used by the data provider trait.
pub mod data_provider {
	/// Alias for the result type of the election data provider.
	pub type Result<T> = sp_std::result::Result<T, &'static str>;
}

/// Something that can provide the data to an [`ElectionProvider`].
pub trait ElectionDataProvider<AccountId, BlockNumber> {
	/// Maximum number of votes per voter that this data provider is providing.
	///
	/// Note that this is the absolute maximum, less votes is also possible.
	const MAXIMUM_VOTES_PER_VOTER: u32;

	/// All possible targets for the election, i.e. the candidates.
	///
	/// If `maybe_max_size` is `Some(v)` then the size of the resulting vector MUST NOT be more than
	/// `v` bytes.
	///
	/// This should be implemented as a self-weighing function. The implementor should register its
	/// appropriate weight at the end of execution with the system pallet directly.
	fn targets(bounds: SnapshotBounds) -> data_provider::Result<Vec<AccountId>>;

	/// All possible voters for the election.
	///
	/// Note that if a notion of self-vote exists, it should be represented here.
	///
	/// If `maybe_max_size` is `Some(v)` then the size of the resulting vector MUST NOT be more than
	/// `v` bytes.
	///
	/// This should be implemented as a self-weighing function. The implementor should register its
	/// appropriate weight at the end of execution with the system pallet directly.
	fn voters(
		bounds: SnapshotBounds,
	) -> data_provider::Result<Vec<(AccountId, VoteWeight, Vec<AccountId>)>>;

	/// The number of targets to elect.
	///
	/// This should be implemented as a self-weighing function. The implementor should register its
	/// appropriate weight at the end of execution with the system pallet directly.
	fn desired_targets() -> data_provider::Result<u32>;

	/// Provide a best effort prediction about when the next election is about to happen.
	///
	/// In essence, the implementor should predict with this function when it will trigger the
	/// [`ElectionProvider::elect`].
	///
	/// This is only useful for stateful election providers.
	fn next_election_prediction(now: BlockNumber) -> BlockNumber;

	/// Utility function only to be used in benchmarking scenarios, to be implemented optionally,
	/// else a noop.
	#[cfg(any(feature = "runtime-benchmarks", test))]
	fn put_snapshot(
		_voters: Vec<(AccountId, VoteWeight, Vec<AccountId>)>,
		_targets: Vec<AccountId>,
		_target_stake: Option<VoteWeight>,
	) {
	}

	/// Utility function only to be used in benchmarking scenarios, to be implemented optionally,
	/// else a noop.
	///
	/// Same as `put_snapshot`, but can add a single voter one by one.
	#[cfg(any(feature = "runtime-benchmarks", test))]
	fn add_voter(_voter: AccountId, _weight: VoteWeight, _targets: Vec<AccountId>) {}

	/// Utility function only to be used in benchmarking scenarios, to be implemented optionally,
	/// else a noop.
	///
	/// Same as `put_snapshot`, but can add a single voter one by one.
	#[cfg(any(feature = "runtime-benchmarks", test))]
	fn add_target(_target: AccountId) {}

	/// Clear all voters and targets.
	#[cfg(any(feature = "runtime-benchmarks", test))]
	fn clear() {}
}

#[cfg(feature = "std")]
impl<AccountId, BlockNumber> ElectionDataProvider<AccountId, BlockNumber> for () {
	const MAXIMUM_VOTES_PER_VOTER: u32 = 0;
	fn targets(_: SnapshotBounds) -> data_provider::Result<Vec<AccountId>> {
		Ok(Default::default())
	}
	fn voters(
		_: SnapshotBounds,
	) -> data_provider::Result<Vec<(AccountId, VoteWeight, Vec<AccountId>)>> {
		Ok(Default::default())
	}
	fn desired_targets() -> data_provider::Result<u32> {
		Ok(Default::default())
	}
	fn next_election_prediction(now: BlockNumber) -> BlockNumber {
		now
	}
}

/// Something that can compute the result of an election and pass it back to the caller.
///
/// This trait only provides an interface to _request_ an election, i.e.
/// [`ElectionProvider::elect`]. That data required for the election need to be passed to the
/// implemented of this trait through [`ElectionProvider::DataProvider`].
pub trait ElectionProvider<AccountId, BlockNumber> {
	/// The error type that is returned by the provider.
	type Error: Debug;

	/// The data provider of the election.
	type DataProvider: ElectionDataProvider<AccountId, BlockNumber>;

	/// Elect a new set of winners.
	///
	/// The result is returned in a target major format, namely as vector of supports.
	///
	/// This should be implemented as a self-weighing function. The implementor should register its
	/// appropriate weight at the end of execution with the system pallet directly.
	fn elect() -> Result<Supports<AccountId>, Self::Error>;
}

#[cfg(feature = "std")]
impl<AccountId, BlockNumber> ElectionProvider<AccountId, BlockNumber> for () {
	type Error = &'static str;
	type DataProvider = ();

	fn elect() -> Result<Supports<AccountId>, Self::Error> {
		Err("<() as ElectionProvider> cannot do anything.")
	}
}

/// A utility trait for something to implement `ElectionDataProvider` in a sensible way.
///
/// This is generic over `AccountId` and it can represent a validator, a nominator, or any other
/// entity.
///
/// To simplify the trait, the `VoteWeight` is hardcoded as the weight of each entity. The weights
/// are ascending, the higher, the better. In the long term, if this trait ends up having use cases
/// outside of the election context, it is easy enough to make it generic over the `VoteWeight`.
///
/// Something that implements this trait will do a best-effort sort over ids, and thus can be
/// used on the implementing side of [`ElectionDataProvider`].
pub trait SortedListProvider<AccountId> {
	/// The list's error type.
	type Error;

	/// An iterator over the list, which can have `take` called on it.
	fn iter() -> Box<dyn Iterator<Item = AccountId>>;

	/// The current count of ids in the list.
	fn count() -> u32;

	/// Return true if the list already contains `id`.
	fn contains(id: &AccountId) -> bool;

	/// Hook for inserting a new id.
	fn on_insert(id: AccountId, weight: VoteWeight) -> Result<(), Self::Error>;

	/// Hook for updating a single id.
	fn on_update(id: &AccountId, weight: VoteWeight);

	/// Hook for removing am id from the list.
	fn on_remove(id: &AccountId);

	/// Regenerate this list from scratch. Returns the count of items inserted.
	///
	/// This should typically only be used at a runtime upgrade.
	fn regenerate(
		all: impl IntoIterator<Item = AccountId>,
		weight_of: Box<dyn Fn(&AccountId) -> VoteWeight>,
	) -> u32;

	/// Remove `maybe_count` number of items from the list. Returns the number of items actually
	/// removed. WARNING: removes all items if `maybe_count` is `None`, which should never be done
	/// in production settings because it can lead to an unbounded amount of storage accesses.
	fn clear(maybe_count: Option<u32>) -> u32;

	/// Sanity check internal state of list. Only meant for debug compilation.
	fn sanity_check() -> Result<(), &'static str>;

	/// If `who` changes by the returned amount they are guaranteed to have a worst case change
	/// in their list position.
	#[cfg(feature = "runtime-benchmarks")]
	fn weight_update_worst_case(_who: &AccountId, _is_increase: bool) -> VoteWeight {
		VoteWeight::MAX
	}
}

/// Something that can provide the `VoteWeight` of an account. Similar to [`ElectionProvider`] and
/// [`ElectionDataProvider`], this should typically be implementing by whoever is supposed to *use*
/// `SortedListProvider`.
pub trait VoteWeightProvider<AccountId> {
	/// Get the current `VoteWeight` of `who`.
	fn vote_weight(who: &AccountId) -> VoteWeight;

	/// For tests and benchmarks, set the `VoteWeight`.
	#[cfg(any(feature = "runtime-benchmarks", test))]
	fn set_vote_weight_of(_: &AccountId, _: VoteWeight) {}
}

/// Something that can compute the result to an NPoS solution.
pub trait NposSolver {
	/// The account identifier type of this solver.
	type AccountId: sp_npos_elections::IdentifierT;
	/// The accuracy of this solver. This will affect the accuracy of the output.
	type Accuracy: PerThing128;
	/// The error type of this implementation.
	type Error: sp_std::fmt::Debug + sp_std::cmp::PartialEq;

	/// Solve an NPoS solution with the given `voters`, `targets`, and select `to_elect` count
	/// of `targets`.
	fn solve(
		to_elect: usize,
		targets: Vec<Self::AccountId>,
		voters: Vec<(Self::AccountId, VoteWeight, Vec<Self::AccountId>)>,
	) -> Result<ElectionResult<Self::AccountId, Self::Accuracy>, Self::Error>;
}

/// A wrapper for [`sp_npos_elections::seq_phragmen`] that implements [`super::NposSolver`]. See the
/// documentation of [`sp_npos_elections::seq_phragmen`] for more info.
pub struct SequentialPhragmen<AccountId, Accuracy, Balancing = ()>(
	sp_std::marker::PhantomData<(AccountId, Accuracy, Balancing)>,
);

impl<
		AccountId: IdentifierT,
		Accuracy: PerThing128,
		Balancing: Get<Option<(usize, ExtendedBalance)>>,
	> NposSolver for SequentialPhragmen<AccountId, Accuracy, Balancing>
{
	type AccountId = AccountId;
	type Accuracy = Accuracy;
	type Error = sp_npos_elections::Error;
	fn solve(
		winners: usize,
		targets: Vec<Self::AccountId>,
		voters: Vec<(Self::AccountId, VoteWeight, Vec<Self::AccountId>)>,
	) -> Result<ElectionResult<Self::AccountId, Self::Accuracy>, Self::Error> {
		sp_npos_elections::seq_phragmen(winners, targets, voters, Balancing::get())
	}
}

/// A wrapper for [`sp_npos_elections::phragmms`] that implements [`NposSolver`]. See the
/// documentation of [`sp_npos_elections::phragmms`] for more info.
pub struct PhragMMS<AccountId, Accuracy, Balancing = ()>(
	sp_std::marker::PhantomData<(AccountId, Accuracy, Balancing)>,
);

impl<
		AccountId: IdentifierT,
		Accuracy: PerThing128,
		Balancing: Get<Option<(usize, ExtendedBalance)>>,
	> NposSolver for PhragMMS<AccountId, Accuracy, Balancing>
{
	type AccountId = AccountId;
	type Accuracy = Accuracy;
	type Error = sp_npos_elections::Error;
	fn solve(
		winners: usize,
		targets: Vec<Self::AccountId>,
		voters: Vec<(Self::AccountId, VoteWeight, Vec<Self::AccountId>)>,
	) -> Result<ElectionResult<Self::AccountId, Self::Accuracy>, Self::Error> {
		sp_npos_elections::phragmms(winners, targets, voters, Balancing::get())
	}
}

/// The limits imposed on a snapshot, either voters or targets.
#[derive(Clone, Copy, RuntimeDebug, scale_info::TypeInfo, Encode, Decode)]
pub struct SnapshotBounds {
	/// The bound on size, in bytes. `None` means unbounded.
	size: Option<u32>,
	/// The bound on count. `None` means unbounded.
	count: Option<u32>,
}

/// Utility builder for [`SnapshotBounds`].
///
/// The main purpose of this is to prevent mixing the order of similarly typed arguments (e.g. u32
/// size and count).
#[derive(Default)]
pub struct SnapshotBoundsBuilder {
	size: Option<u32>,
	count: Option<u32>,
}

impl SnapshotBoundsBuilder {
	/// Set the given size.
	pub fn size(mut self, size: u32) -> Self {
		self.size = Some(size);
		self
	}

	/// Set the given count.
	pub fn count(mut self, count: u32) -> Self {
		self.count = Some(count);
		self
	}

	/// Build the [`SnapshotBounds`] instance.
	pub fn build(self) -> SnapshotBounds {
		SnapshotBounds { size: self.size, count: self.count }
	}
}

impl SnapshotBounds {
	/// Create a new instance of self, with size limit.
	pub const fn new_size(size: u32) -> Self {
		Self { size: Some(size), count: None }
	}

	/// Create a new instance of self, with count limit.
	pub const fn new_count(count: u32) -> Self {
		Self { count: Some(count), size: None }
	}

	/// Create a new unbounded instance of self.
	pub const fn new_unbounded() -> Self {
		Self { size: None, count: None }
	}

	/// returns true if `given_size` exhausts `self.size`.
	pub fn size_exhausted(&self, given_size: impl FnOnce() -> u32) -> bool {
		self.size.map_or(false, |size| given_size() > size)
	}

	/// returns true if `given_count` exhausts `self.count`.
	pub fn count_exhausted(&self, given_count: impl FnOnce() -> u32) -> bool {
		self.count.map_or(false, |count| given_count() > count)
	}

	/// Returns true if `self` is exhausted by either of `given_size` and `given_count`.
	///
	/// Note that this will return `false` against an empty contains (size = 1, count = 0). Calling
	/// [`self.size_exhausted`] alone cannot handle this edge case, since no information of the
	/// count is available.
	///
	/// # Warning
	///
	/// The function name is hinting at the correct order of `given_size` and `given_count`. Be
	/// aware that they have the same type, and mixing them can be catastrophic.
	pub fn exhausts_size_count_non_zero(
		&self,
		given_size: impl FnOnce() -> u32,
		given_count: impl FnOnce() -> u32,
	) -> bool {
		// take care of this pesky edge case: empty vector (size = 1, count = 0) should not exhaust
		// anything.
		let given_size = given_size();
		let given_count = given_count();
		if given_size == 1 || given_count == 0 {
			return false
		}
		self.size_exhausted(|| given_size) || self.count_exhausted(|| given_count)
	}

	/// Return the size bound, if one exists.
	pub fn size_bound(&self) -> Option<usize> {
		self.size.map(|b| b as usize)
	}

	/// Return the count bound, if one exists.
	pub fn count_bound(&self) -> Option<usize> {
		self.count.map(|b| b as usize)
	}

	/// Return `true` if self is fully unbounded.
	pub fn is_unbounded(&self) -> bool {
		self.size.is_none() && self.count.is_none()
	}

	/// Return `true` if either of the bounds exists.
	pub fn is_bounded(&self) -> bool {
		!self.is_unbounded()
	}

	/// Predict the `::with_capacity` of a collection that has `self` as bounds (size and count),
	/// when each item is `item_size` bytes on average.
	///
	/// Returns `None` if no capacity could be made (e.g. if `self` is unbounded).
	pub fn predict_capacity(&self, item_size: usize) -> Option<usize> {
		match (self.size_bound(), self.count_bound()) {
			(Some(max_size), Some(max_count)) => Some(max_count.min(max_size / item_size.max(1))),
			(Some(max_size), None) => Some(max_size / item_size.max(1)),
			(None, Some(max_count)) => Some(max_count),
			(None, None) => None,
		}
	}
}
