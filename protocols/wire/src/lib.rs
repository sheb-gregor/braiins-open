// Copyright (C) 2019  Braiins Systems s.r.o.
//
// This file is part of Braiins Open-Source Initiative (BOSI).
//
// BOSI is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// Please, keep in mind that we may also license BOSI or any part thereof
// under a proprietary license. For more information on the terms and conditions
// of such proprietary license or if you have any other questions, please
// contact us at opensource@braiins.com.

#[cfg(all(feature = "tokio03", feature = "tokio02"))]
compile_error!("You can't use both Tokio 0.3 and 0.2. Note: The `tokio02` feature requires default features to be turned off");

#[cfg(feature = "tokio03")]
pub(crate) use tokio;
#[cfg(feature = "tokio03")]
pub(crate) use tokio_util;

#[cfg(feature = "tokio02")]
pub(crate) use tokio02_ as tokio;
#[cfg(feature = "tokio02")]
pub(crate) use tokio02_util as tokio_util;

mod connection;
pub use connection::*;

mod server;
pub use server::*;

mod client;
pub use client::*;

mod framing;
pub use framing::*;
