// Copyright Istio Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod boring;

use std::sync::Arc;

pub use crate::tls::boring::*;
use hyper::http::uri::InvalidUri;

#[derive(thiserror::Error, Debug, Clone)]
pub enum Error {
    #[error("invalid operation: {0:?}")]
    SslError(#[from] ErrorStack),

    #[error("invalid root certificate: {0}")]
    InvalidRootCert(ErrorStack),

    #[error("invalid uri: {0}")]
    InvalidUri(#[from] Arc<InvalidUri>),
}

impl From<InvalidUri> for Error {
    fn from(err: InvalidUri) -> Self {
        Error::InvalidUri(Arc::new(err))
    }
}
