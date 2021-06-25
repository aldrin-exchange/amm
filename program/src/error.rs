//! Error types

use num_derive::FromPrimitive;
use solana_program::{decode_error::DecodeError, program_error::ProgramError};
use thiserror::Error;

/// Errors that may be returned by the TokenSwap program.
#[derive(Clone, Debug, Eq, Error, FromPrimitive, PartialEq)]
pub enum SwapError {
    /// 0. The account cannot be initialized because it is already being used.
    #[error("Swap account already in use")]
    AlreadyInUse,
    /// 1. The program address provided doesn't match the value generated by the program.
    #[error("Invalid program address generated from nonce and key")]
    InvalidProgramAddress,
    /// 2. The owner of the input isn't set to the program address generated by the program.
    #[error("Input account owner is not the program address")]
    InvalidOwner,
    /// 3. The owner of the pool token output is set to the program address generated by the program.
    #[error("Output pool account owner cannot be the program address")]
    InvalidOutputOwner,
    /// 4. The deserialization of the account returned something besides State::Mint.
    #[error("Deserialized account is not an SPL Token mint")]
    ExpectedMint,
    /// 5. The deserialization of the account returned something besides State::Account.
    #[error("Deserialized account is not an SPL Token account")]
    ExpectedAccount,
    /// 6. The input token account is empty.
    #[error("Input token account empty")]
    EmptySupply,
    /// 7. The pool token mint has a non-zero supply.
    #[error("Pool token mint has a non-zero supply")]
    InvalidSupply,
    /// 8. The provided token account has a delegate.
    #[error("Token account has a delegate")]
    InvalidDelegate,
    /// 9. The input token is invalid for swap.
    #[error("InvalidInput")]
    InvalidInput,
    /// 10. Address of the provided swap token account is incorrect.
    #[error("Address of the provided swap token account is incorrect")]
    IncorrectSwapAccount,
    /// 11. Address of the provided pool token mint is incorrect
    #[error("Address of the provided pool token mint is incorrect")]
    IncorrectPoolMint,
    /// 12. The output token is invalid for swap.
    #[error("InvalidOutput")]
    InvalidOutput,
    /// 13. General calculation failure due to overflow or underflow
    #[error("General calculation failure due to overflow or underflow")]
    CalculationFailure,
    /// 14. Invalid instruction number passed in.
    #[error("Invalid instruction")]
    InvalidInstruction,
    /// 15. Swap input token accounts have the same mint
    #[error("Swap input token accounts have the same mint")]
    RepeatedMint,
    /// 16. Swap instruction exceeds desired slippage limit
    #[error("Swap instruction exceeds desired slippage limit")]
    ExceededSlippage,
    /// 17. The provided token account has a close authority.
    #[error("Token account has a close authority")]
    InvalidCloseAuthority,
    /// 18. The pool token mint has a freeze authority.
    #[error("Pool token mint has a freeze authority")]
    InvalidFreezeAuthority,
    /// 19. The pool fee token account is incorrect
    #[error("Pool fee token account incorrect")]
    IncorrectFeeAccount,
    /// 20. Given pool token amount results in zero trading tokens
    #[error("Given pool token amount results in zero trading tokens")]
    ZeroTradingTokens,
    /// 21. The fee calculation failed due to overflow, underflow, or unexpected 0
    #[error("Fee calculation failed due to overflow, underflow, or unexpected 0")]
    FeeCalculationFailure,
    /// 22. ConversionFailure
    #[error("Conversion to u64 failed with an overflow or underflow")]
    ConversionFailure,
    /// 23. The provided fee does not match the program owner's constraints
    #[error("The provided fee does not match the program owner's constraints")]
    InvalidFee,
    /// The provided token program does not match the token program expected by the swap
    #[error("The provided token program does not match the token program expected by the swap")]
    IncorrectTokenProgramId,
    /// The provided curve type is not supported by the program owner
    #[error("The provided curve type is not supported by the program owner")]
    UnsupportedCurveType,
    /// The provided curve parameters are invalid
    #[error("The provided curve parameters are invalid")]
    InvalidCurve,
    /// The operation cannot be performed on the given curve
    #[error("The operation cannot be performed on the given curve")]
    UnsupportedCurveOperation,
}
impl From<SwapError> for ProgramError {
    fn from(e: SwapError) -> Self {
        ProgramError::Custom(e as u32)
    }
}
impl<T> DecodeError<T> for SwapError {
    fn type_of() -> &'static str {
        "Swap Error"
    }
}

/// Errors that may be returned by the farming instructions of the TokenSwap program.
#[derive(Clone, Debug, Eq, Error, FromPrimitive, PartialEq)]
pub enum FarmingError {
    /// Farming funds cannot be withdrawn before the minimum withdrawal time has passed
    #[error("Cannot withdraw before the minimum withdrawal time has passed")]
    MinimumWithdrawalTimeNotPassed,
    ///Got no farming tokens ready for withdrawal
    #[error("Got no farming tokens ready for withdrawal")]
    NoTokensToWithdraw,
    ///Got an error from Farming token calculation
    #[error("Got an error from Farming token calculation")]
    FarmingTokenCalculationError,
    ///Got no tokens to unlock from the last snapshot to the current one
    #[error("No tokens to unlock")]
    CannotSnapshotNoTokensToUnlock,
    ///Got no tokens to unlock as they cannot be allocated to no one
    #[error("No tokens frozen")]
    CannotSnapshotNoTokensFrozen,

}
impl From<FarmingError> for ProgramError {
    fn from(e: FarmingError) -> Self {
        ProgramError::Custom(100 + e as u32)
    }
}
impl<T> DecodeError<T> for FarmingError {
    fn type_of() -> &'static str {
        "Farming Error"
    }
}