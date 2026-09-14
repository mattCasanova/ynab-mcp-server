//! Thin client over the YNAB REST API (https://api.ynab.com/v1).
//! Only the endpoints the tools need; every amount stays in milliunits here.

use std::sync::Mutex;

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::money::Milliunits;

const BASE_URL: &str = "https://api.ynab.com/v1";

#[derive(Debug, Error)]
pub enum YnabError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("token contains characters that are not valid in an HTTP header")]
    BadToken,
    #[error("YNAB API error {status}: {name} — {detail}")]
    Api {
        status: u16,
        name: String,
        detail: String,
    },
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    data: T,
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ErrorDetail,
}

#[derive(Debug, Deserialize)]
struct ErrorDetail {
    name: String,
    detail: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PlanSummary {
    pub id: String,
    pub name: String,
    pub last_modified_on: Option<String>,
    pub first_month: Option<String>,
    pub last_month: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PlansData {
    plans: Vec<PlanSummary>,
}

#[derive(Debug, Deserialize)]
struct PlanData {
    plan: PlanSummary,
}

#[derive(Debug, Deserialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub on_budget: bool,
    pub closed: bool,
    pub note: Option<String>,
    pub balance: Milliunits,
    pub cleared_balance: Milliunits,
    pub uncleared_balance: Milliunits,
    pub last_reconciled_at: Option<String>,
    pub deleted: bool,
}

#[derive(Debug, Deserialize)]
struct AccountsData {
    accounts: Vec<Account>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Category {
    pub id: String,
    pub category_group_id: String,
    pub category_group_name: Option<String>,
    pub name: String,
    pub hidden: bool,
    pub deleted: bool,
    pub note: Option<String>,
    pub budgeted: Milliunits,
    pub activity: Milliunits,
    pub balance: Milliunits,
    pub goal_type: Option<String>,
    pub goal_target: Option<Milliunits>,
    pub goal_target_date: Option<String>,
    pub goal_percentage_complete: Option<i64>,
    pub goal_under_funded: Option<Milliunits>,
    pub goal_overall_left: Option<Milliunits>,
}

#[derive(Debug, Deserialize)]
pub struct CategoryGroup {
    pub id: String,
    pub name: String,
    pub hidden: bool,
    pub deleted: bool,
    pub categories: Vec<Category>,
}

#[derive(Debug, Deserialize)]
struct CategoriesData {
    category_groups: Vec<CategoryGroup>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MonthDetail {
    pub month: String,
    pub note: Option<String>,
    pub income: Milliunits,
    pub budgeted: Milliunits,
    pub activity: Milliunits,
    pub to_be_budgeted: Milliunits,
    pub age_of_money: Option<i64>,
    pub categories: Vec<Category>,
}

#[derive(Debug, Deserialize)]
struct MonthData {
    month: MonthDetail,
}

#[derive(Debug, Deserialize)]
pub struct SubTransaction {
    pub amount: Milliunits,
    pub memo: Option<String>,
    pub payee_name: Option<String>,
    pub category_id: Option<String>,
    pub category_name: Option<String>,
    pub transfer_account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MoneyMovement {
    pub month: String,
    pub moved_at: Option<String>,
    pub note: Option<String>,
    pub from_category_id: Option<String>,
    pub to_category_id: Option<String>,
    pub amount: Milliunits,
}

#[derive(Debug, Deserialize)]
struct MoneyMovementsData {
    money_movements: Vec<MoneyMovement>,
}

/// Covers both TransactionDetail and HybridTransaction (category/payee endpoints).
#[derive(Debug, Deserialize)]
pub struct Transaction {
    pub id: String,
    pub date: String,
    pub amount: Milliunits,
    pub memo: Option<String>,
    pub cleared: String,
    pub approved: bool,
    pub flag_color: Option<String>,
    pub account_id: String,
    pub account_name: Option<String>,
    pub payee_id: Option<String>,
    pub payee_name: Option<String>,
    pub category_id: Option<String>,
    pub category_name: Option<String>,
    pub transfer_account_id: Option<String>,
    pub import_id: Option<String>,
    pub deleted: bool,
    #[serde(default)]
    pub subtransactions: Vec<SubTransaction>,
}

#[derive(Debug, Deserialize)]
struct TransactionsData {
    transactions: Vec<Transaction>,
}

#[derive(Debug, Deserialize)]
pub struct ScheduledTransaction {
    pub id: String,
    pub date_first: String,
    pub date_next: String,
    pub frequency: String,
    pub amount: Milliunits,
    pub memo: Option<String>,
    pub account_name: Option<String>,
    pub payee_name: Option<String>,
    pub category_name: Option<String>,
    pub deleted: bool,
}

#[derive(Debug, Deserialize)]
struct ScheduledTransactionsData {
    scheduled_transactions: Vec<ScheduledTransaction>,
}

#[derive(Debug, Serialize)]
pub struct SaveTransaction {
    pub account_id: String,
    pub date: String,
    pub amount: Milliunits,
    pub payee_name: Option<String>,
    pub category_id: Option<String>,
    pub memo: Option<String>,
    pub cleared: &'static str,
    pub approved: bool,
    pub import_id: String,
}

#[derive(Debug, Serialize)]
struct PostTransactions<'a> {
    transactions: &'a [SaveTransaction],
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SaveTransactionsResult {
    pub transaction_ids: Vec<String>,
    #[serde(default)]
    pub duplicate_import_ids: Vec<String>,
    /// The created transactions, used to map ids back to what was requested.
    #[serde(default, skip_serializing)]
    pub transactions: Vec<Transaction>,
}

pub enum TransactionKind {
    Uncategorized,
    Unapproved,
}

impl TransactionKind {
    fn as_query(&self) -> &'static str {
        match self {
            Self::Uncategorized => "uncategorized",
            Self::Unapproved => "unapproved",
        }
    }
}

#[derive(Default)]
pub struct TransactionFilter {
    pub account_id: Option<String>,
    pub category_id: Option<String>,
    pub since_date: Option<String>,
    pub until_date: Option<String>,
    pub kind: Option<TransactionKind>,
}

pub struct Client {
    http: reqwest::Client,
    plan_id: String,
    /// Last observed `X-Rate-Limit` header, e.g. "12/200".
    rate_limit: Mutex<Option<String>>,
}

impl Client {
    pub fn new(access_token: &str, plan_id: String) -> Result<Self, YnabError> {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {access_token}"))
            .map_err(|_| YnabError::BadToken)?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        let http = reqwest::Client::builder()
            .user_agent(concat!("ynab-mcp/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .build()?;
        Ok(Self {
            http,
            plan_id,
            rate_limit: Mutex::new(None),
        })
    }

    pub fn plan_id(&self) -> &str {
        &self.plan_id
    }

    pub fn rate_limit(&self) -> Option<String> {
        self.rate_limit.lock().expect("rate limit lock").clone()
    }

    fn plan_url(&self, path: &str) -> String {
        format!("{BASE_URL}/plans/{}{path}", self.plan_id)
    }

    async fn send<T: DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<T, YnabError> {
        let resp = req.send().await?;
        if let Some(raw) = resp.headers().get("x-rate-limit") {
            match raw.to_str() {
                Ok(limit) => {
                    *self.rate_limit.lock().expect("rate limit lock") = Some(limit.to_string())
                }
                Err(e) => tracing::warn!(error = %e, "x-rate-limit header is not text; ignoring"),
            }
        }
        let status = resp.status();
        if !status.is_success() {
            let body = match resp.text().await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, %status, "could not read error body");
                    format!("(unreadable error body: {e})")
                }
            };
            let (name, detail) = match serde_json::from_str::<ErrorEnvelope>(&body) {
                Ok(e) => (e.error.name, e.error.detail),
                Err(e) => {
                    tracing::warn!(error = %e, %status, "error body is not the YNAB error shape");
                    ("unknown".to_string(), body)
                }
            };
            tracing::warn!(%status, name, "YNAB API error");
            return Err(YnabError::Api {
                status: status.as_u16(),
                name,
                detail,
            });
        }
        let envelope: Envelope<T> = resp.json().await?;
        Ok(envelope.data)
    }

    async fn get<T: DeserializeOwned>(
        &self,
        url: String,
        query: &[(&str, String)],
    ) -> Result<T, YnabError> {
        self.send(self.http.get(url).query(query)).await
    }

    pub async fn plans(&self) -> Result<Vec<PlanSummary>, YnabError> {
        let data: PlansData = self.get(format!("{BASE_URL}/plans"), &[]).await?;
        Ok(data.plans)
    }

    /// The concrete plan id behind "last-used" or "default", so caches are keyed per plan.
    pub async fn resolve_plan_id(&self) -> Result<String, YnabError> {
        let data: PlanData = self.get(self.plan_url(""), &[]).await?;
        Ok(data.plan.id)
    }

    pub async fn accounts(&self) -> Result<Vec<Account>, YnabError> {
        let data: AccountsData = self.get(self.plan_url("/accounts"), &[]).await?;
        Ok(data.accounts)
    }

    pub async fn category_groups(&self) -> Result<Vec<CategoryGroup>, YnabError> {
        let data: CategoriesData = self.get(self.plan_url("/categories"), &[]).await?;
        Ok(data.category_groups)
    }

    /// Every money movement in the plan; the API has no date filter, callers filter by month.
    pub async fn money_movements(&self) -> Result<Vec<MoneyMovement>, YnabError> {
        let data: MoneyMovementsData = self.get(self.plan_url("/money_movements"), &[]).await?;
        Ok(data.money_movements)
    }

    pub async fn month(&self, month: &str) -> Result<MonthDetail, YnabError> {
        let data: MonthData = self
            .get(self.plan_url(&format!("/months/{month}")), &[])
            .await?;
        Ok(data.month)
    }

    pub async fn transactions(
        &self,
        filter: &TransactionFilter,
    ) -> Result<Vec<Transaction>, YnabError> {
        let url = match (&filter.account_id, &filter.category_id) {
            (Some(account), _) => self.plan_url(&format!("/accounts/{account}/transactions")),
            (None, Some(category)) => {
                self.plan_url(&format!("/categories/{category}/transactions"))
            }
            (None, None) => self.plan_url("/transactions"),
        };
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(d) = &filter.since_date {
            query.push(("since_date", d.clone()));
        }
        if let Some(d) = &filter.until_date {
            query.push(("until_date", d.clone()));
        }
        if let Some(k) = &filter.kind {
            query.push(("type", k.as_query().to_string()));
        }
        let data: TransactionsData = self.get(url, &query).await?;
        Ok(data.transactions)
    }

    pub async fn scheduled_transactions(&self) -> Result<Vec<ScheduledTransaction>, YnabError> {
        let data: ScheduledTransactionsData = self
            .get(self.plan_url("/scheduled_transactions"), &[])
            .await?;
        Ok(data.scheduled_transactions)
    }

    pub async fn delete_transaction(&self, transaction_id: &str) -> Result<(), YnabError> {
        let url = self.plan_url(&format!("/transactions/{transaction_id}"));
        let _: serde_json::Value = self.send(self.http.delete(url)).await?;
        Ok(())
    }

    pub async fn create_transactions(
        &self,
        transactions: &[SaveTransaction],
    ) -> Result<SaveTransactionsResult, YnabError> {
        let body = PostTransactions { transactions };
        self.send(self.http.post(self.plan_url("/transactions")).json(&body))
            .await
    }
}
