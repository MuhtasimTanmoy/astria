use anyhow::Result;
use async_trait::async_trait;
use serde::{
    Deserialize,
    Serialize,
};

use crate::{
    accounts::state_ext::{
        Address,
        Balance,
        Nonce,
        StateReadExt,
        StateWriteExt,
    },
    transaction::ActionHandler,
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Transaction {
    to: Address,
    from: Address,
    amount: Balance,
    nonce: Nonce,
}

impl Transaction {
    pub fn new(to: Address, from: Address, amount: Balance, nonce: Nonce) -> Self {
        Self {
            to,
            from,
            amount,
            nonce,
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(self)?;
        Ok(bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let tx = serde_json::from_slice(bytes)?;
        Ok(tx)
    }
}

#[async_trait]
impl ActionHandler for Transaction {
    fn check_stateless(&self) -> Result<()> {
        Ok(())
    }

    async fn check_stateful<S: StateReadExt + 'static>(&self, state: &S) -> Result<()> {
        let curr_nonce = state.get_account_nonce(&self.from).await?;

        // TODO: do nonces start at 0 or 1? this assumes an account's first tx has nonce 1.
        if curr_nonce >= self.nonce {
            anyhow::bail!("invalid nonce");
        }

        let curr_balance = state.get_account_balance(&self.from).await?;
        if curr_balance < self.amount {
            anyhow::bail!("insufficient funds");
        }

        Ok(())
    }

    async fn execute<S: StateWriteExt>(&self, state: &mut S) -> Result<()> {
        let from_balance = state.get_account_balance(&self.from).await?;
        let from_nonce = state.get_account_nonce(&self.from).await?;
        let to_balance = state.get_account_balance(&self.to).await?;
        state.put_account_balance(&self.from, from_balance - self.amount);
        state.put_account_nonce(&self.from, from_nonce + 1);
        state.put_account_balance(&self.to, to_balance + self.amount);
        Ok(())
    }
}