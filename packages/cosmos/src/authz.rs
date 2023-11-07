use chrono::{DateTime, Utc};
use cosmos_sdk_proto::cosmos::{
    authz::v1beta1::{
        GenericAuthorization, Grant, GrantAuthorization, MsgExec, MsgGrant,
        QueryGranterGrantsRequest, QueryGranterGrantsResponse,
    },
    base::query::v1beta1::{PageRequest, PageResponse},
};
use prost::Message;
use prost_types::Timestamp;

use crate::{error::Action, Address, Cosmos, HasAddress, TypedMessage};

impl From<MsgGrant> for TypedMessage {
    fn from(msg: MsgGrant) -> Self {
        TypedMessage::new(cosmos_sdk_proto::Any {
            type_url: "/cosmos.authz.v1beta1.MsgGrant".to_owned(),
            value: msg.encode_to_vec(),
        })
    }
}

impl From<MsgExec> for TypedMessage {
    fn from(msg: MsgExec) -> Self {
        TypedMessage::new(cosmos_sdk_proto::Any {
            type_url: "/cosmos.authz.v1beta1.MsgExec".to_owned(),
            value: msg.encode_to_vec(),
        })
    }
}

/// A message for granting authorization to another address.
pub struct MsgGrantHelper {
    /// Address granting permissions
    pub granter: Address,
    /// Address receiving permissions
    pub grantee: Address,
    /// Which features are being authorized
    pub authorization: String,
    /// When the authorization expires
    pub expiration: Option<DateTime<Utc>>,
}

impl From<MsgGrantHelper> for TypedMessage {
    fn from(value: MsgGrantHelper) -> Self {
        MsgGrant::from(value).into()
    }
}

impl From<MsgGrantHelper> for MsgGrant {
    fn from(
        MsgGrantHelper {
            granter,
            grantee,
            authorization,
            expiration,
        }: MsgGrantHelper,
    ) -> Self {
        let authorization = GenericAuthorization { msg: authorization };
        let authorization = prost_types::Any {
            type_url: "/cosmos.authz.v1beta1.GenericAuthorization".to_owned(),
            value: authorization.encode_to_vec(),
        };
        MsgGrant {
            granter: granter.get_address_string(),
            grantee: grantee.get_address_string(),
            grant: Some(Grant {
                authorization: Some(authorization),
                expiration: expiration.map(datetime_to_timestamp),
            }),
        }
    }
}

fn datetime_to_timestamp(x: DateTime<Utc>) -> Timestamp {
    prost_types::Timestamp {
        seconds: x.timestamp(),
        nanos: x
            .timestamp_subsec_nanos()
            .try_into()
            .expect("DateTime<Utc>'s nanos is too large"),
    }
}

impl Cosmos {
    /// Check which grants the given address has authorized.
    pub async fn query_granter_grants(
        &self,
        granter: impl HasAddress,
    ) -> anyhow::Result<Vec<GrantAuthorization>> {
        let mut res = vec![];
        let mut pagination = None;

        loop {
            let req = QueryGranterGrantsRequest {
                granter: granter.get_address_string(),
                pagination: pagination.take(),
            };

            let QueryGranterGrantsResponse {
                mut grants,
                pagination: pag_res,
            } = self
                .perform_query(req, Action::QueryGranterGrants(granter.get_address()), true)
                .await?
                .into_inner();
            println!("{grants:?}");
            if grants.is_empty() {
                break Ok(res);
            }

            res.append(&mut grants);

            pagination = match pag_res {
                Some(PageResponse { next_key, total: _ }) => Some(PageRequest {
                    key: next_key,
                    // Ideally we'd just leave this out so we use next_key
                    // instead, but the Rust types don't allow this
                    offset: res.len().try_into()?,
                    limit: 10,
                    count_total: false,
                    reverse: false,
                }),
                None => None,
            };
        }
    }
}
