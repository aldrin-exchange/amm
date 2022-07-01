use crate::prelude::*;
use std::fmt::Display;

#[error_code]
pub enum AmmError {
    #[msg("Operation would result in an overflow")]
    MathOverflow,
    /// Use this error via the [`acc`] function to provide more background
    /// about the issue.
    #[msg("Provided account breaks some constraints, see logs for more info")]
    InvalidAccountInput,
    #[msg("One of the provided input arguments is invalid")]
    InvalidArg,
    /// Use this error for program paths which should never be reached if the
    /// program logic works as intended.
    #[msg("There's a bug in the program, see logs for more info")]
    InvariantViolation,
    /// Use this error whenever trying to interact with a pool, but providing
    /// wrong token mints
    #[msg("Current mints are not available on current pool")]
    InvalidTokenMints,
    /// Invalid specification of pool token vault and user token wallet
    #[msg("Invalid specification of pool token vault and user token wallet")]
    InvalidTokenVaultWalletSpecification,
    /// Invalid account owner
    #[msg("Invalid account owner, does not coincide with user")]
    InvalidAccountOwner,
    /// Invalid token amount to deposit
    #[msg("Invalid token amount to deposit")]
    InvalidTokenAmount,
}

pub fn acc(msg: impl Display) -> AmmError {
    msg!("[InvalidAccountInput] {}", msg);

    AmmError::InvalidAccountInput
}

pub fn arg(msg: impl Display) -> AmmError {
    msg!("[InvalidArg] {}", msg);

    AmmError::InvalidArg
}
