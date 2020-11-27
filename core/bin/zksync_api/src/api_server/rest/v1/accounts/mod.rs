//! Accounts part of API implementation.

// Built-in uses
pub use self::types::{AccountInfo, AccountState, DepositingBalances, DepositingFunds};

// External uses
use actix_web::{
    web::{self, Json},
    Scope,
};

// Workspace uses
use zksync_config::ConfigurationOptions;
use zksync_storage::QueryResult;
use zksync_types::{Address, BlockNumber, TokenId};

// Local uses
use crate::{core_api_client::CoreApiClient, utils::token_db_cache::TokenDBCache};

use self::types::{
    AccountQuery, AccountReceiptsQuery, AccountTxReceipt, PendingAccountTxReceipt, SearchDirection,
    TxLocation,
};
use super::{ApiError, JsonResult};

mod client;
#[cfg(test)]
mod tests;
mod types;

fn unable_to_find_token(token_id: TokenId) -> anyhow::Error {
    anyhow::anyhow!("Unable to find token with ID {}", token_id)
}

// Additional parser because actix-web doesn't understand enums in path extractor.
fn parse_account_query(query: String) -> Result<AccountQuery, ApiError> {
    query.parse().map_err(|err| {
        dbg!(&err);
        ApiError::bad_request("Must be specified either an account ID or an account address.")
            .detail(format!("An error occurred: {}", err))
    })
}

async fn find_account_address(
    data: &web::Data<ApiAccountsData>,
    query: String,
) -> Result<Address, ApiError> {
    let query = parse_account_query(query)?;

    data.account_address(query)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| {
            ApiError::bad_request("Unable to find account.")
                .detail(format!("Given account {:?} is absent", query))
        })
}

/// Shared data between `api/v1/accounts` endpoints.
#[derive(Clone)]
struct ApiAccountsData {
    tokens: TokenDBCache,
    core_api_client: CoreApiClient,
    confirmations_for_eth_event: BlockNumber,
}

impl ApiAccountsData {
    fn new(
        tokens: TokenDBCache,
        core_api_client: CoreApiClient,
        confirmations_for_eth_event: BlockNumber,
    ) -> Self {
        Self {
            tokens,
            core_api_client,
            confirmations_for_eth_event,
        }
    }

    async fn account_address(&self, query: AccountQuery) -> QueryResult<Option<Address>> {
        let address = match query {
            AccountQuery::Id(id) => {
                let mut storage = self.tokens.pool.access_storage().await?;

                let account_state = storage.chain().account_schema().account_state(id).await?;

                account_state
                    .committed
                    .map(|(_id, account)| account.address)
            }
            AccountQuery::Address(address) => Some(address),
        };

        Ok(address)
    }

    async fn account_info(&self, query: AccountQuery) -> QueryResult<Option<AccountInfo>> {
        let mut storage = self.tokens.pool.access_storage().await?;

        let account_state = storage
            .chain()
            .account_schema()
            .account_state(query)
            .await?;

        // TODO This code uses same logic as the old RPC, but I'm not sure that if it is correct.
        let (account_id, account) = if let Some(state) = account_state.committed {
            state
        } else {
            // This account has not been committed.
            return Ok(None);
        };

        let committed = AccountState::from_storage(&account, &self.tokens).await?;
        let verified = match account_state.verified {
            Some(state) => AccountState::from_storage(&state.1, &self.tokens).await?,
            None => AccountState::default(),
        };

        let depositing = {
            let ongoing_ops = self
                .core_api_client
                .get_unconfirmed_deposits(account.address)
                .await?;

            DepositingBalances::from_pending_ops(
                ongoing_ops,
                self.confirmations_for_eth_event,
                &self.tokens,
            )
            .await?
        };

        let info = AccountInfo {
            address: account.address,
            id: account_id,
            committed,
            verified,
            depositing,
        };

        Ok(Some(info))
    }

    async fn tx_receipts(
        &self,
        address: Address,
        location: TxLocation,
        direction: SearchDirection,
        limit: u32,
    ) -> QueryResult<Vec<AccountTxReceipt>> {
        let mut storage = self.tokens.pool.access_storage().await?;

        let location = (location.block as u64, location.index);

        let items = storage
            .chain()
            .operations_ext_schema()
            .get_account_transactions_history_from(
                &address,
                location,
                direction.into(),
                limit as u64,
            )
            .await?;

        dbg!(&items);

        Ok(items.into_iter().map(AccountTxReceipt::from).collect())
    }

    async fn pending_tx_receipts(
        &self,
        address: Address,
    ) -> QueryResult<Vec<PendingAccountTxReceipt>> {
        let ongoing_ops = self
            .core_api_client
            .get_unconfirmed_deposits(address)
            .await?;

        let receipts = ongoing_ops
            .into_iter()
            .map(|(block_id, op)| PendingAccountTxReceipt::from_priority_op(block_id, op))
            .collect();

        Ok(receipts)
    }
}

// Server implementation

async fn account_info(
    data: web::Data<ApiAccountsData>,
    web::Path(query): web::Path<String>,
) -> JsonResult<Option<AccountInfo>> {
    let query = parse_account_query(query)?;

    data.account_info(query)
        .await
        .map(Json)
        .map_err(ApiError::internal)
}

async fn account_receipts(
    data: web::Data<ApiAccountsData>,
    web::Path(account_query): web::Path<String>,
    web::Query(location_query): web::Query<AccountReceiptsQuery>,
) -> JsonResult<Vec<AccountTxReceipt>> {
    let (location, direction, limit) = location_query.validate()?;

    let address = find_account_address(&data, account_query).await?;

    let receipts = data
        .tx_receipts(address, location, direction, limit)
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(receipts))
}

async fn account_pending_receipts(
    data: web::Data<ApiAccountsData>,
    web::Path(account_query): web::Path<String>,
) -> JsonResult<Vec<PendingAccountTxReceipt>> {
    let address = find_account_address(&data, account_query).await?;

    let receipts = data
        .pending_tx_receipts(address)
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(receipts))
}

pub fn api_scope(
    env_options: &ConfigurationOptions,
    tokens: TokenDBCache,
    core_api_client: CoreApiClient,
) -> Scope {
    let data = ApiAccountsData::new(
        tokens,
        core_api_client,
        env_options.confirmations_for_eth_event as BlockNumber,
    );

    web::scope("accounts")
        .data(data)
        .route("{id}", web::get().to(account_info))
        .route("{id}/receipts", web::get().to(account_receipts))
        .route(
            "{id}/receipts/pending",
            web::get().to(account_pending_receipts),
        )
}
