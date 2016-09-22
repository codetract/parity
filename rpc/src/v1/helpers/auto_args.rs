// Copyright 2015, 2016 Ethcore (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Automatically serialize and deserialize parameters around a strongly-typed function.

// because we reuse the type names as idents in the macros as a dirty hack to
// work around `concat_idents!` being unstable.
#![allow(non_snake_case)]

use super::errors;
use v1::types::BlockNumber;

use jsonrpc_core::{Error, Params, Value, from_params, to_value};
use serde::{Serialize, Deserialize};

/// A wrapper type without an implementation of `Deserialize`
/// which allows a special implementation of `Wrap` for functions
/// that take a default block parameter.
pub struct BlockParam(BlockNumber);

/// Wrapper trait for RPC functions.
pub trait Wrap<B: Send + Sync + 'static> {
	fn wrap_rpc(&self, base: &B, params: Params) -> Result<Value, Error>;
}

// special impl for no parameters.
impl<B, OUT> Wrap<B> for fn(&B) -> Result<OUT, Error>
	where B: Send + Sync + 'static, OUT: Serialize
{
	fn wrap_rpc(&self, base: &B, params: Params) -> Result<Value, Error> {
		::v1::helpers::params::expect_no_params(params)
			.and_then(|()| (self)(base))
			.map(to_value)
	}
}

// creates a wrapper implementation which deserializes the parameters,
// calls the function with concrete type, and serializes the output.
macro_rules! wrap {
	($($x: ident),+) => {
		impl <
			BASE: Send + Sync + 'static,
			OUT: Serialize,
			$($x: Deserialize,)+
		> Wrap<BASE> for fn(&BASE, $($x,)+) -> Result<OUT, Error> {
			fn wrap_rpc(&self, base: &BASE, params: Params) -> Result<Value, Error> {
				from_params::<($($x,)+)>(params).and_then(|($($x,)+)| {
					(self)(base, $($x,)+)
				}).map(to_value)
			}
		}
	}
}

// special impl for no parameters other than block parameter.
impl<B, OUT> Wrap<B> for fn(&B, BlockParam) -> Result<OUT, Error>
	where B: Send + Sync + 'static, OUT: Serialize
{
	fn wrap_rpc(&self, base: &B, params: Params) -> Result<Value, Error> {
		let len = match params {
			Params::Array(ref v) => v.len(),
			_ => return Err(errors::invalid_params("not an array", "")),
		};

		let (id,) = match len {
			0 => (BlockNumber::Latest,),
			1 => try!(from_params::<(BlockNumber,)>(params)),
			_ => return Err(Error::invalid_params()),
		};

		(self)(base, BlockParam(id)).map(to_value)
	}
}

// similar to `wrap!`, but handles the Default Block Parameter.
// accepts an additional argument indicating the number of non-block parameters.
macro_rules! wrap_with_block_param {
	($num: expr, $($x: ident),+) => {
		impl <
			BASE: Send + Sync + 'static,
			OUT: Serialize,
			$($x: Deserialize,)+
		> Wrap<BASE> for fn(&BASE, $($x,)+ BlockParam) -> Result<OUT, Error> {
			fn wrap_rpc(&self, base: &BASE, params: Params) -> Result<Value, Error> {
				let len = match params {
					Params::Array(ref v) => v.len(),
					_ => return Err(errors::invalid_params("not an array", "")),
				};

				let params = match len - $num {
					0 => from_params::<($($x,)+)>(params)
						.map(|($($x,)+)| ($($x,)+ BlockNumber::Latest)),
					1 => from_params::<($($x,)+ BlockNumber)>(params)
						.map(|($($x,)+ id)| ($($x,)+ id)),
					_ => Err(Error::invalid_params()),
				};

				let ($($x,)+ id) = try!(params);
				(self)(base, $($x,)+ BlockParam(id)).map(to_value)
			}
		}
	}
}

wrap!(A, B, C, D, E);
wrap!(A, B, C, D);
wrap!(A, B, C);
wrap!(A, B);
wrap!(A);

wrap_with_block_param!(5, A, B, C, D, E);
wrap_with_block_param!(4, A, B, C, D);
wrap_with_block_param!(3, A, B, C);
wrap_with_block_param!(2, A, B);
wrap_with_block_param!(1, A);